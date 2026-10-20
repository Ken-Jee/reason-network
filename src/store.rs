//! SQLite: messages, and the sessions this machine has seen: live ones, ones gone for less than a day, and the
//! person's own mailboxes (`cli`, such as `terminal`). Files that came with messages are saved in the inbox folder, one
//! folder per message, and go with their message.

use crate::{
    files,
    proto::{Attachment, Message, Tier},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub struct Store {
    db: Connection,
    inbox: PathBuf,
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
    "id, sender, sender_agent, sender_repo, recipient, via, reply_to, body, wake, sent_at, approval, files";
const SESSION_COLS: &str = "name, key, agent, cwd, repo, branch, doing, tier, last_seen";

impl Store {
    /// The database at `path`, and the folder `inbox` that messages' files are saved in.
    pub fn open(path: &Path, inbox: PathBuf) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        // Added in 0.10.7.
        if !db.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'approval'")?.exists([])? {
            db.execute("ALTER TABLE messages ADD COLUMN approval TEXT", [])?;
        }
        // Added with files: their names, and their text while the message is held (once saved, it is in the inbox
        // folder instead).
        if !db.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'files'")?.exists([])? {
            db.execute("ALTER TABLE messages ADD COLUMN files TEXT", [])?;
        }
        Ok(Store { db, inbox })
    }

    /// A message's own folder in the inbox folder. Message ids are plain letters and digits (`clean` holds a peer's to
    /// that), so this is always just inside it.
    fn files_dir(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            anyhow::bail!("malformed message id");
        }
        Ok(self.inbox.join(id))
    }

    /// Stores a message; false if one with this id is already here (a retried delivery). Unless it is held, its files
    /// are saved in its inbox folder first, and `m` then lists them by path.
    pub fn insert(&self, m: &mut Message, held: bool) -> Result<bool> {
        if self.has(&m.id)? {
            return Ok(false);
        }
        let saved = match (held, m.files.is_empty()) {
            (false, false) => {
                let dir = self.files_dir(&m.id)?;
                // Left by an attempt that didn't finish.
                let _ = std::fs::remove_dir_all(&dir);
                files::save(&dir, &m.files)?;
                Some(dir)
            }
            _ => None,
        };
        let n = self.db.execute(
            &format!(
                "INSERT OR IGNORE INTO messages ({MESSAGE_COLS}, held) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
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
                files_col(&m.files, held),
                held
            ],
        );
        match (n, saved) {
            (Ok(n), Some(dir)) => {
                m.files = files::located(&dir, &m.files);
                Ok(n == 1)
            }
            (Ok(n), None) => Ok(n == 1),
            (Err(e), dir) => {
                if let Some(dir) = dir {
                    let _ = std::fs::remove_dir_all(dir);
                }
                Err(e.into())
            }
        }
    }

    pub fn message(&self, id: &str) -> Result<Option<Message>> {
        let sql = format!("SELECT {MESSAGE_COLS}, held FROM messages WHERE id = ?1");
        Ok(self.db.query_row(&sql, [id], |r| self.row(r)).optional()?)
    }

    pub fn has(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM messages WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
    }

    pub fn unread(&self, recipient: &str) -> Result<Vec<Message>> {
        let mut st = self.db.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS}, held FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0 ORDER BY sent_at, id"
        ))?;
        let rows = st.query_map([recipient], |r| self.row(r))?;
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
        let mut st = self.db.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS}, held FROM messages WHERE held = 1 ORDER BY sent_at, id"
        ))?;
        let rows = st.query_map([], |r| self.row(r))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether a message's inbox folder stays: while the message is unread, and for a day after it was read. Held
    /// messages have none, and a message that is gone takes its files with it.
    fn keep_files(&self, id: &str, now: i64) -> bool {
        let read_at =
            self.db.query_row("SELECT read_at FROM messages WHERE id = ?1", [id], |r| r.get::<_, Option<i64>>(0));
        match read_at.optional() {
            Ok(Some(None)) => true,
            Ok(Some(Some(at))) => at > now - files::KEEP_MS,
            Ok(None) => false,
            // Not sure: keep it for now.
            Err(_) => true,
        }
    }

    /// Deletes the inbox folders of messages that are gone, or were read more than a day ago.
    pub fn sweep_files(&self, now: i64) {
        let Ok(entries) = std::fs::read_dir(&self.inbox) else { return };
        for e in entries.flatten() {
            if !self.keep_files(&e.file_name().to_string_lossy(), now) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    fn drop_files(&self, id: &str) {
        if let Ok(dir) = self.files_dir(id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Approves (`release`) or drops held messages; returns the ones affected. An approved message's files are saved
    /// now (one that can't be saved stays held); a dropped one's never are.
    pub fn settle_held(&self, ids: &[String], release: bool) -> Result<Vec<Message>> {
        let held = self.held()?;
        let mut picked: Vec<Message> = held.into_iter().filter(|m| ids.is_empty() || ids.contains(&m.id)).collect();
        for m in &mut picked {
            if !release {
                self.db.execute("DELETE FROM messages WHERE id = ?1", [&m.id])?;
                continue;
            }
            let dir = self.files_dir(&m.id)?;
            if !m.files.is_empty() {
                let _ = std::fs::remove_dir_all(&dir);
                files::save(&dir, &m.files)?;
            }
            let col = files_col(&m.files, false);
            if let Err(e) =
                self.db.execute("UPDATE messages SET held = 0, files = ?2 WHERE id = ?1", params![m.id, col])
            {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e.into());
            }
            m.files = files::located(&dir, &m.files);
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
            let unread = "FROM messages WHERE recipient = ?1 AND read_at IS NULL AND held = 0";
            let to = format!("{person}/{name}");
            let mut ids = self.db.prepare_cached(&format!("SELECT id {unread}"))?;
            let ids: Vec<String> = ids.query_map([&to], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            self.db.execute(&format!("DELETE {unread}"), [&to])?;
            for id in ids {
                self.drop_files(&id);
            }
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

impl Store {
    fn row(&self, r: &Row) -> rusqlite::Result<Message> {
        message(r, &self.inbox)
    }
}

fn message(r: &Row, inbox: &Path) -> rusqlite::Result<Message> {
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
        files: {
            let id: String = r.get(0)?;
            let files: Vec<Attachment> =
                r.get::<_, Option<String>>(11)?.and_then(|f| serde_json::from_str(&f).ok()).unwrap_or_default();
            // A held message's files aren't saved yet: their text is here. Once released, they are in its inbox folder.
            if r.get::<_, bool>(12)? { files } else { files::located(&inbox.join(&id), &files) }
        },
    })
}

/// The `files` column: names, and the text too while held.
fn files_col(files: &[Attachment], held: bool) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let kept: Vec<Attachment> = files
        .iter()
        .map(|f| Attachment {
            name: f.name.clone(),
            content: if held { f.content.clone() } else { String::new() },
            path: None,
        })
        .collect();
    Some(serde_json::to_string(&kept).expect("files serialize"))
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
        let store = Store::open(Path::new(":memory:"), temp_inbox()).unwrap();
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
            files: Vec::new(),
        };
        store.insert(&mut to("1", "gone"), false).unwrap();
        store.insert(&mut to("2", "recent"), false).unwrap();
        store.insert(&mut to("3", "terminal"), false).unwrap();
        store.insert(&mut to("4", "gone"), true).unwrap();
        store.forget_sessions(day + 1, &HashSet::from(["still-up"]), "lars").unwrap();
        let names: Vec<String> = store.sessions().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["recent", "still-up", "terminal"]);
        assert_eq!(store.unread_count("lars/gone").unwrap(), 0);
        assert_eq!(store.unread_count("lars/recent").unwrap(), 1);
        assert_eq!(store.unread_count("lars/terminal").unwrap(), 1);
        assert_eq!(store.held().unwrap().len(), 1);
    }

    fn temp_inbox() -> PathBuf {
        std::env::temp_dir().join(format!("reason-network-inbox-{}", ulid::Ulid::new()))
    }

    fn with_files(id: &str, to: &str) -> Message {
        let file = |name: &str, content: &str| Attachment { name: name.into(), content: content.into(), path: None };
        Message {
            id: id.into(),
            from: "sven/claude".into(),
            from_agent: None,
            from_repo: None,
            to: to.into(),
            via: None,
            reply_to: None,
            body: "see the files".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: vec![file("notes.md", "# notes\n"), file("run.log", "ok\n")],
        }
    }

    /// The files of a message as saved: names, whether each is readable by its user and executable by anyone.
    fn saved(dir: &Path) -> Vec<(String, String, bool)> {
        let mut out: Vec<(String, String, bool)> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| {
                        #[cfg(unix)]
                        let exec =
                            std::os::unix::fs::PermissionsExt::mode(&e.metadata().unwrap().permissions()) & 0o111 != 0;
                        #[cfg(not(unix))]
                        let exec = false;
                        (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap(), exec)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    fn stored_text(store: &Store, id: &str) -> String {
        store.db.query_row("SELECT files FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn files_are_saved_with_their_message_and_kept_out_of_the_database() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        let mut m = with_files("01A", "lars/api");
        assert!(store.insert(&mut m, false).unwrap());
        let dir = inbox.join("01A");
        let want =
            vec![("notes.md".to_string(), "# notes\n".to_string(), false), ("run.log".into(), "ok\n".into(), false)];
        assert_eq!(saved(&dir), want);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("notes.md")).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // The message now lists them by path, here and when read back; the database keeps only their names.
        let paths: Vec<Option<String>> = m.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            [Some(dir.join("notes.md").display().to_string()), Some(dir.join("run.log").display().to_string())]
        );
        assert!(m.files.iter().all(|f| f.content.is_empty()));
        assert_eq!(store.unread("lars/api").unwrap()[0].files, m.files);
        assert!(!stored_text(&store, "01A").contains("notes\\n"));
        // A retried delivery changes nothing.
        std::fs::write(dir.join("run.log"), "kept").unwrap();
        assert!(!store.insert(&mut with_files("01A", "lars/api"), false).unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("run.log")).unwrap(), "kept");
        // An id that isn't plain letters and digits never names a folder.
        assert!(store.insert(&mut with_files("../../x", "lars/api"), false).is_err());
        assert!(!inbox.parent().unwrap().join("x").exists());
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn a_held_messages_files_are_saved_only_once_approved() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        store.insert(&mut with_files("01H", "lars/api"), true).unwrap();
        store.insert(&mut with_files("01D", "lars/api"), true).unwrap();
        assert!(!inbox.join("01H").exists() && !inbox.join("01D").exists());
        // Held, the text is kept to save later (and shown to the person by name, with no path).
        let held = store.held().unwrap();
        assert_eq!(held[0].files[0].content, "# notes\n");
        assert_eq!(held[0].files[0].path, None);
        // Dropped: nothing is ever written.
        assert_eq!(store.settle_held(&["01D".into()], false).unwrap().len(), 1);
        assert!(!inbox.join("01D").exists() && store.message("01D").unwrap().is_none());
        // Approved: saved now, and listed by path.
        let released = store.settle_held(&["01H".into()], true).unwrap();
        assert_eq!(saved(&inbox.join("01H")).len(), 2);
        assert_eq!(released[0].files[0].path, Some(inbox.join("01H").join("notes.md").display().to_string()));
        assert_eq!(store.unread("lars/api").unwrap()[0].files, released[0].files);
        assert!(!stored_text(&store, "01H").contains("notes\\n"));
        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn files_go_with_their_message() {
        let inbox = temp_inbox();
        let store = Store::open(Path::new(":memory:"), inbox.clone()).unwrap();
        let day = 86_400_000;
        let session = |name: &str| Remembered {
            name: name.into(),
            key: name.into(),
            agent: "claude-code".into(),
            cwd: "~".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen: 0,
        };
        store.upsert_session(&session("gone")).unwrap();
        store.upsert_session(&session("api")).unwrap();
        for (id, to) in
            [("01GONE", "lars/gone"), ("01UNREAD", "lars/api"), ("01RECENT", "lars/api"), ("01OLD", "lars/api")]
        {
            store.insert(&mut with_files(id, to), false).unwrap();
        }
        std::fs::create_dir_all(inbox.join("01ORPHAN")).unwrap();
        // The unread mail of a session forgotten after a day goes, and its files with it.
        store.forget_sessions(day, &HashSet::from(["api"]), "lars").unwrap();
        assert!(!inbox.join("01GONE").exists());
        // Read files stay a day; unread ones until read; a folder with no message goes.
        let now = 10 * day;
        store.mark_read(&["01RECENT".into()], now - day / 2).unwrap();
        store.mark_read(&["01OLD".into()], now - day - 1).unwrap();
        store.sweep_files(now);
        let left: HashSet<String> = std::fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, HashSet::from(["01UNREAD".to_string(), "01RECENT".to_string()]));
        std::fs::remove_dir_all(&inbox).unwrap();
    }
}
