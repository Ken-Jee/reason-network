//! Sessions running on this machine: registered by `reason network mcp` (a connection), or by hooks (no connection,
//! kept alive by hook events and the agent's process).

use super::{Daemon, Inner, Live, log};
use crate::{agent, git::GitInfo, proto::*, render, store::Remembered, util};
use anyhow::{Result, anyhow, bail};
use std::{collections::HashSet, sync::Arc, time::Instant};

/// A hook-only session that hasn't fired a hook for this long is considered gone.
const HOOK_TTL_MS: i64 = 6 * 3_600_000;
/// A session gone this long isn't coming back: it is forgotten, with the mail it never read.
const FORGET_MS: i64 = 24 * 3_600_000;

impl Daemon {
    /// How fast hooks let us reach an agent's session (`push_failed`: its push channel didn't work).
    fn hook_tier(&self, agent: &str, push_failed: bool) -> Tier {
        match agent {
            "codex" if !push_failed && agent::find_bin("codex").is_some() => Tier::Push,
            // Their plugin or extension delivers into idle sessions; Reason's CLI sends into its sessions.
            "opencode" | "pi" | "reason" => Tier::Push,
            _ => Tier::Turn,
        }
    }

    /// Registers (or refreshes) a session. Returns its mail that is waiting, for the caller to deliver.
    pub(super) fn register(self: &Arc<Self>, conn: Option<u64>, reg: SessionReg, git: GitInfo) -> Result<Registered> {
        let (registered, pending) = self.register_quiet(conn, reg, git)?;
        let key = pending.0;
        if !pending.1.is_empty() {
            let mut g = self.inner.lock().unwrap();
            self.deliver(&mut g, &key, pending.1, true);
        }
        Ok(registered)
    }

    fn register_quiet(
        &self,
        conn: Option<u64>,
        mut reg: SessionReg,
        git: GitInfo,
    ) -> Result<(Registered, (String, Vec<Message>))> {
        let now = util::now_ms();
        let others = self.my_names_elsewhere();
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let key = reg.key.clone();
        if let Some(l) = g.live.get_mut(&key) {
            // Known already (e.g. from a hook, now the MCP server connects, or a reconnect).
            if conn.is_some() {
                l.conn = conn;
            }
            l.reg.tier = if l.push_failed { l.reg.tier.max(Tier::Turn) } else { l.reg.tier.min(reg.tier) };
            if l.last_hook.is_none() {
                l.reg.cwd = reg.cwd;
                l.git = git;
            }
            l.reg.pid = reg.pid.or(l.reg.pid);
        } else {
            if conn.is_none() {
                reg.tier = reg.tier.min(self.hook_tier(&reg.agent, false));
            }
            let name = pick_name(g, &reg, &git, &others)?;
            g.live.insert(
                key.clone(),
                Live {
                    reg,
                    name,
                    conn,
                    git,
                    doing: None,
                    since: now,
                    status: None,
                    last_hook: None,
                    push_failed: false,
                    transcript: None,
                    retry: None,
                    stop_gen: 0,
                },
            );
            self.presence_changed();
        }
        let l = &g.live[&key];
        g.store.upsert_session(&remembered(l, now))?;
        let name = l.name.clone();
        let address = self.addr(&name);
        let pending: Vec<Message> = g.pending(&address)?;
        let wake: Vec<Message> = pending.iter().filter(|m| m.wake).cloned().collect();
        log(&format!("session {address} ({key}) registered"));
        Ok((Registered { name, address, unread: pending.len() as u32 }, (key, wake)))
    }

    /// Session names in use on my other machines, so my addresses stay unique across them.
    fn my_names_elsewhere(&self) -> HashSet<String> {
        let Some(net) = self.net() else { return HashSet::new() };
        let Some(team) = self.team_state() else { return HashSet::new() };
        net.remotes()
            .into_iter()
            .filter(|(id, _)| team.member(id).is_some_and(|m| m.person == self.person))
            .flat_map(|(_, r)| r.presence.sessions.into_iter().map(|s| s.name))
            .collect()
    }

    pub(super) fn set_status(&self, key: &str, doing: Option<String>, name: Option<String>) -> Result<Registered> {
        let others = self.my_names_elsewhere();
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let new_name = match name.as_deref().map(util::slug) {
            Some(n) if n.is_empty() => bail!("a name needs letters or digits"),
            Some(n) if g.live.iter().any(|(k, l)| l.name == n && k != key) || others.contains(&n) => {
                bail!("'{n}' is taken by a live session")
            }
            Some(n) if n == self.person => bail!("'{n}' is your person name; pick another session name"),
            n => n,
        };
        let live = g.live.get_mut(key).ok_or_else(|| anyhow!("this session is not registered"))?;
        if let Some(d) = doing {
            let d = d.trim();
            live.doing = (!d.is_empty()).then(|| d.chars().take(200).collect());
        }
        if let Some(n) = new_name
            && n != live.name
        {
            g.store.rename(
                &live.name,
                &n,
                &format!("{}/{}", self.person, live.name),
                &format!("{}/{n}", self.person),
            )?;
            live.name = n;
        }
        g.store.upsert_session(&remembered(live, util::now_ms()))?;
        let address = self.addr(&live.name);
        Ok(Registered { name: live.name.clone(), unread: g.store.unread_count(&address)?, address })
    }

    pub(super) fn unread(&self, key: &str) -> Result<Unread> {
        let mut g = self.inner.lock().unwrap();
        let Some(l) = g.live.get(key) else { return Ok(Unread { count: 0, from: Vec::new() }) };
        let address = self.addr(&l.name);
        let msgs = g.pending(&address)?;
        let mut from: Vec<String> = Vec::new();
        for m in &msgs {
            if !from.contains(&m.from) {
                from.push(m.from.clone());
            }
        }
        Ok(Unread { count: msgs.len() as u32, from })
    }

    /// A hook fired in an agent: keep its session alive, track busy/idle, hand back mail to inject.
    pub(super) async fn hook(self: &Arc<Self>, h: HookCall) -> Result<HookReply> {
        let HookCall { agent, event, session_id, cwd, pid, mail, transcript, doing, name } = h;
        let (agent, event) = (agent.as_str(), event.as_str());
        let key = format!("{agent}:{session_id}");
        let now = util::now_ms();
        if event == "end" {
            let mut g = self.inner.lock().unwrap();
            if g.live.get(&key).is_some_and(|l| l.conn.is_none())
                && let Some(l) = g.live.remove(&key)
            {
                let _ = g.store.touch_session(&l.name, now);
                log(&format!("session {} ({key}) ended", self.addr(&l.name)));
                self.presence_changed();
            }
            return Ok(HookReply::default());
        }
        let known = self.inner.lock().unwrap().live.contains_key(&key);
        if !known {
            let remembered_cwd = self.inner.lock().unwrap().store.session_by_key(&key)?.map(|r| r.cwd);
            let cwd = cwd.or(remembered_cwd).unwrap_or_else(|| "/".into());
            let git = self.git(&cwd).await;
            let reg = SessionReg {
                key: key.clone(),
                agent: agent.to_string(),
                cwd,
                agent_session_id: Some(session_id.clone()),
                pid,
                tier: Tier::Pull,
                name,
            };
            self.register_quiet(None, reg, git)?;
        }

        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let Some(l) = g.live.get_mut(&key) else { return Ok(HookReply::default()) };
        if let Some(t) = transcript {
            l.transcript = Some(t.into());
        }
        // Only a report: it says nothing about the session's activity or how to reach it.
        if event == "doing" {
            if let Some(d) = doing {
                l.doing = Some(d.trim().chars().take(200).collect::<String>()).filter(|d| !d.is_empty());
            }
            return Ok(HookReply::default());
        }
        l.last_hook = Some(now);
        l.reg.tier = l.reg.tier.min(self.hook_tier(agent, l.push_failed));
        if pid.is_some() {
            l.reg.pid = pid;
        }
        let status = if matches!(event, "prompt" | "tool") { "busy" } else { "idle" };
        if l.status.as_ref().is_none_or(|(s, _)| s != status) {
            l.status = Some((status.to_string(), now));
        }
        let address = self.addr(&l.name);
        let agent_kind = l.reg.agent.clone();
        // Mid-turn, only what should interrupt; at a pause, everything.
        let msgs: Vec<Message> = if mail {
            g.pending(&address)?.into_iter().filter(|m| event != "tool" || m.wake).collect()
        } else {
            Vec::new()
        };
        // Not marked read here: the hook confirms with `Delivered` once it has printed them. Until then they are in
        // flight, so a hook asking at the same moment (opencode 2.x can run two plugin instances) doesn't get them too.
        let ids: Vec<String> = msgs.iter().map(|m| m.id.clone()).collect();
        for id in &ids {
            g.in_flight.insert(id.clone(), Instant::now());
        }
        let text = (!msgs.is_empty()).then(|| render::for_agent(&msgs, Some(&agent_kind), false));
        Ok(HookReply { address: Some(address), text, ids })
    }

    /// The hook-known session of `agent` that is calling a tool right now, when that is unambiguous: the only
    /// busy one in `cwd` (the caller is mid-turn), else the only busy one at all.
    pub(super) fn attach(&self, agent: &str, cwd: &str) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        let busy: Vec<(&String, &Live)> = g
            .live
            .iter()
            .filter(|(_, l)| {
                l.reg.agent == agent && l.last_hook.is_some() && l.status.as_ref().is_some_and(|(s, _)| s == "busy")
            })
            .collect();
        let here: Vec<&(&String, &Live)> = busy.iter().filter(|(_, l)| l.reg.cwd == cwd).collect();
        match (here.as_slice(), busy.as_slice()) {
            ([(k, _)], _) => Some((*k).clone()),
            ([], [(k, _)]) => Some((*k).clone()),
            _ => None,
        }
    }

    pub(super) fn drop_conn(&self, conn: u64) {
        let now = util::now_ms();
        let mut g = self.inner.lock().unwrap();
        g.conns.remove(&conn);
        let mine: Vec<String> = g.live.iter().filter(|(_, l)| l.conn == Some(conn)).map(|(k, _)| k.clone()).collect();
        for key in mine {
            let l = g.live.get_mut(&key).expect("listed above");
            // Hooks still vouch for it: keep it, reachable through them.
            if l.last_hook.is_some() && l.reg.pid.is_some_and(util::pid_alive) {
                l.conn = None;
                continue;
            }
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
                log(&format!("session {} ({key}) left", self.addr(&l.name)));
                self.presence_changed();
            }
        }
    }

    /// Forgets hook-only sessions whose agent is gone or has been silent too long.
    pub(super) fn prune(&self, g: &mut Inner) {
        let now = util::now_ms();
        let dead: Vec<String> = g
            .live
            .iter()
            .filter(|(_, l)| {
                l.conn.is_none()
                    && (l.reg.pid.is_some_and(|p| !util::pid_alive(p))
                        || l.last_hook.is_none_or(|t| now - t > HOOK_TTL_MS))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in dead {
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
            }
        }
        let live: HashSet<&str> = g.live.values().map(|l| l.name.as_str()).collect();
        let _ = g.store.forget_sessions(now - FORGET_MS, &live, &self.person);
    }
}

fn remembered(l: &Live, now: i64) -> Remembered {
    Remembered {
        name: l.name.clone(),
        key: l.reg.key.clone(),
        agent: l.reg.agent.clone(),
        cwd: l.reg.cwd.clone(),
        repo: l.git.repo.clone(),
        branch: l.git.branch.clone(),
        doing: l.doing.clone(),
        tier: l.reg.tier,
        last_seen: now,
    }
}

/// Reuse this session's previous name, else `<repo>-<agent>` (inheriting it and its mail if it is stale),
/// else the first free `-2`, `-3`, …
fn pick_name(g: &Inner, reg: &SessionReg, git: &GitInfo, elsewhere: &HashSet<String>) -> Result<String> {
    let taken = |n: &str| g.live.values().any(|l| l.name == n) || elsewhere.contains(n);
    if let Some(r) = g.store.session_by_key(&reg.key)?
        && !taken(&r.name)
    {
        return Ok(r.name);
    }
    let base = reg
        .name
        .as_deref()
        .map(util::slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| agent::default_name(&reg.agent, &reg.cwd, git));
    if !taken(&base) {
        return Ok(base);
    }
    Ok((2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).expect("some suffix is free"))
}
