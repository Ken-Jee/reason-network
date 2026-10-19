//! SQLite: messages, and the sessions this machine has seen: live ones, ones gone for less than a day, and the
//! person's own mailboxes (`cli`, such as `terminal`).

use crate::proto::{Message, Tier};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::{collections::HashSet, path::Path};

pub struct Store {
    db: Connection,
}

#[derive(Debug, Clone)]
pub struct Remembered {
    pub name: String,
    pub key: String,
    pub agent: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub doing: Option<String>,
    pub tier: Tier,
    pub last_seen: i64,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    sender TEXT NOT NULL,
    sender_agent TEXT,
    sender_repo TEXT,
    recipient TEXT NOT NULL,
    via TEXT,
    reply_to TEXT,
    body TEXT NOT NULL,
    wake INTEGER NOT NULL,
    sent_at INTEGER NOT NULL,
    read_at INTEGER,
    held INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS messages_unread ON messages (recipient, read_at);
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    msg TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    name TEXT PRIMARY KEY,
    key TEXT NOT NULL,
    agent TEXT NOT NULL,
    cwd TEXT NOT NULL,
    repo TEXT,
    branch TEXT,
    doing TEXT,
    tier TEXT NOT NULL,
    last_seen INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_key ON sessions (key);
";

const MESSAGE_COLS: &str =
    "id, sender, sender_agent, sender_repo, recipient, via, reply_to, body, wake, sent_at, approval";
const SESSION_COLS: &str = "name, key, agent, cwd, repo, branch, doing, tier, last_seen";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        // Added in 0.10.7.
        if !db.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'approval'")?.exists([])? {
            db.execute("ALTER TABLE messages ADD COLUMN approval TEXT", [])?;
        }
        Ok(Store { db })
    }

    /// Stores a message; false if one with this id is already here (a retried delivery).
    pub fn insert(&self, m: &Message, held: bool) -> Result<bool> {
        let n = self.db.execute(
            &format!(
                "INSERT OR IGNORE INTO messages ({MESSAGE_COLS}, held) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            ),
            params![
                m.id,
                m.from,
                m.from_agent,
                m.from_repo,
                m.to,
                m.via,
                m.reply_to,
                m.body,
                m.wake,
                m.sent_at,
                m.approval.as_ref().map(|a| serde_json::to_string(a).expect("approvals serialize")),
                held
            ],
        )?;
        Ok(n == 1)
    }

    pub fn message(&self, id: &str) -> Result<Option<Message>> {
        let sql = format!("SELECT {MESSAGE_COLS} FROM messages WHERE id = ?1");
        Ok(self.db.query_row(&sql, [id], message).optional()?)
    }

    pub fn has(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM messages WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
    }

    pub fn unread(&self, recipient: &str) -> Result<Vec<Message>> {
        let mut st = self.db.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0 ORDER BY sent_at, id"
        ))?;
        let rows = st.query_map([recipient], message)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unread_count(&self, recipient: &str) -> Result<u32> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0",
            [recipient],
            |r| r.get(0),
        )?)
    }

    pub fn held(&self) -> Result<Vec<Message>> {
        let mut st = self
            .db
            .prepare_cached(&format!("SELECT {MESSAGE_COLS} FROM messages WHERE held = 1 ORDER BY sent_at, id"))?;
        let rows = st.query_map([], message)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Approves (`release`) or drops held messages; returns the ones affected.
    pub fn settle_held(&self, ids: &[String], release: bool) -> Result<Vec<Message>> {
        let held = self.held()?;
        let picked: Vec<Message> = held.into_iter().filter(|m| ids.is_empty() || ids.contains(&m.id)).collect();
        let sql =
            if release { "UPDATE messages SET held = 0 WHERE id = ?1" } else { "DELETE FROM messages WHERE id = ?1" };
        let mut st = self.db.prepare_cached(sql)?;
        for m in &picked {
            st.execute([&m.id])?;
        }
        Ok(picked)
    }

    pub fn outbox_add(&self, target: &str, m: &Message) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO outbox (id, target, msg, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![m.id, target, serde_json::to_string(m)?, m.sent_at],
        )?;
        Ok(())
    }

    /// Undelivered messages for a machine; older than a week are given up on.
    pub fn outbox_for(&self, target: &str, now: i64) -> Result<Vec<Message>> {
        self.db.execute("DELETE FROM outbox WHERE created_at < ?1", [now - 7 * 86_400_000])?;
        let mut st = self.db.prepare_cached("SELECT msg FROM outbox WHERE target = ?1 ORDER BY created_at, id")?;
        let rows = st.query_map([target], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok())).collect())
    }

    pub fn outbox_done(&self, id: &str) -> Result<()> {
        self.db.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    pub fn prefixed(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.db.prepare_cached("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?1 || x'ff'")?;
        let rows = st.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_prefix(&self, prefix: &str) -> Result<()> {
        self.db.execute("DELETE FROM kv WHERE key >= ?1 AND key < ?1 || x'ff'", [prefix])?;
        Ok(())
    }

    pub fn mark_read(&self, ids: &[String], now: i64) -> Result<()> {
        let mut st = self.db.prepare_cached("UPDATE messages SET read_at = ?2 WHERE id = ?1 AND read_at IS NULL")?;
        for id in ids {
            st.execute(params![id, now])?;
        }
        Ok(())
    }

    /// A renamed session keeps its unread mail.
    pub fn rename(&self, old_name: &str, new_name: &str, old_addr: &str, new_addr: &str) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET recipient = ?2 WHERE recipient = ?1 AND read_at IS NULL",
            params![old_addr, new_addr],
        )?;
        self.db.execute("DELETE FROM sessions WHERE name = ?1", [new_name])?;
        self.db.execute("UPDATE sessions SET name = ?2 WHERE name = ?1", params![old_name, new_name])?;
        Ok(())
    }

    pub fn upsert_session(&self, s: &Remembered) -> Result<()> {
        self.db.execute(
            &format!(
                "INSERT INTO sessions ({SESSION_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (name) DO UPDATE SET key = ?2, agent = ?3, cwd = ?4, repo = ?5, branch = ?6,
                 doing = ?7, tier = ?8, last_seen = ?9"
            ),
            params![s.name, s.key, s.agent, s.cwd, s.repo, s.branch, s.doing, s.tier.as_str(), s.last_seen],
        )?;
        Ok(())
    }

    pub fn touch_session(&self, name: &str, now: i64) -> Result<()> {
        self.db.execute("UPDATE sessions SET last_seen = ?2 WHERE name = ?1", params![name, now])?;
        Ok(())
    }

    /// Forgets the sessions not seen since `before`, other than `live` ones and the person's own mailboxes (`cli`), and
    /// deletes the mail they never read, so a session that takes the name later doesn't get it (mail held for the
    /// person stays theirs to settle).
    pub fn forget_sessions(&self, before: i64, live: &HashSet<&str>, person: &str) -> Result<()> {
        let mut st = self.db.prepare_cached("SELECT name FROM sessions WHERE last_seen < ?1 AND agent != 'cli'")?;
        let gone: Vec<String> = st.query_map([before], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for name in gone.iter().filter(|n| !live.contains(n.as_str())) {
            self.db.execute(
                "DELETE FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0",
                [format!("{person}/{name}")],
            )?;
            self.db.execute("DELETE FROM sessions WHERE name = ?1", [name])?;
        }
        Ok(())
    }

    pub fn session_by_key(&self, key: &str) -> Result<Option<Remembered>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE key = ?1 ORDER BY last_seen DESC LIMIT 1");
        Ok(self.db.query_row(&sql, [key], remembered).optional()?)
    }

    pub fn session_by_name(&self, name: &str) -> Result<Option<Remembered>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE name = ?1");
        Ok(self.db.query_row(&sql, [name], remembered).optional()?)
    }

    pub fn sessions(&self) -> Result<Vec<Remembered>> {
        let mut st = self.db.prepare_cached(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY name"))?;
        let rows = st.query_map([], remembered)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn message(r: &Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        from: r.get(1)?,
        from_agent: r.get(2)?,
        from_repo: r.get(3)?,
        to: r.get(4)?,
        via: r.get(5)?,
        reply_to: r.get(6)?,
        body: r.get(7)?,
        wake: r.get(8)?,
        notify: false,
        sent_at: r.get(9)?,
        approval: r.get::<_, Option<String>>(10)?.and_then(|a| serde_json::from_str(&a).ok()),
    })
}

fn remembered(r: &Row) -> rusqlite::Result<Remembered> {
    Ok(Remembered {
        name: r.get(0)?,
        key: r.get(1)?,
        agent: r.get(2)?,
        cwd: r.get(3)?,
        repo: r.get(4)?,
        branch: r.get(5)?,
        doing: r.get(6)?,
        tier: Tier::parse(&r.get::<_, String>(7)?),
        last_seen: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_gone_a_day_is_forgotten_with_the_mail_it_never_read() {
        // (Not one that is live, nor the person's own mailbox, nor mail held for the person.)
        let store = Store::open(Path::new(":memory:")).unwrap();
        let day = 86_400_000;
        let session = |name: &str, agent: &str, last_seen: i64| Remembered {
            name: name.into(),
            key: name.into(),
            agent: agent.into(),
            cwd: "~".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen,
        };
        for s in [
            session("gone", "claude-code", 0),
            session("recent", "claude-code", 2 * day),
            session("still-up", "claude-code", 0),
            session("terminal", "cli", 0),
        ] {
            store.upsert_session(&s).unwrap();
        }
        let to = |id: &str, name: &str| Message {
            id: id.into(),
            from: "sven/claude".into(),
            from_agent: None,
            from_repo: None,
            to: format!("lars/{name}"),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: false,
            notify: false,
            sent_at: 0,
            approval: None,
        };
        store.insert(&to("1", "gone"), false).unwrap();
        store.insert(&to("2", "recent"), false).unwrap();
        store.insert(&to("3", "terminal"), false).unwrap();
        store.insert(&to("4", "gone"), true).unwrap();
        store.forget_sessions(day + 1, &HashSet::from(["still-up"]), "lars").unwrap();
        let names: Vec<String> = store.sessions().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["recent", "still-up", "terminal"]);
        assert_eq!(store.unread_count("lars/gone").unwrap(), 0);
        assert_eq!(store.unread_count("lars/recent").unwrap(), 1);
        assert_eq!(store.unread_count("lars/terminal").unwrap(), 1);
        assert_eq!(store.held().unwrap().len(), 1);
    }
}
