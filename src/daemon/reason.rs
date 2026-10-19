//! Reason sessions. Reason's agent runs in its cloud, so there is no hook or MCP server here: when `reason` is on
//! (`reason network setup`), the daemon lists your live Reason sessions (`reason session list --json`) every few seconds,
//! and delivers to them with `reason session send`. They reply through the Reason Network CLI, which runs here.
//!
//! Where the Reason CLI manages this engine, the daemon also keeps the CLI's `reason network __relay` running: it
//! carries the workspace's cloud agents' calls (Reason's hosted tools) onto the network, for as long as the daemon runs.

use super::{Daemon, log};
use crate::{agent, git::GitInfo, proto::*, util};
use anyhow::{Result, bail};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc, time::Duration};

const EVERY: Duration = Duration::from_secs(15);
/// The relay is started again this long after it stops, doubling up to `RELAY_MAX_WAIT` while it keeps stopping.
const RELAY_WAIT: Duration = Duration::from_secs(30);
const RELAY_MAX_WAIT: Duration = Duration::from_secs(600);

impl Daemon {
    pub(super) async fn reason_loop(self: Arc<Self>) {
        let mut failing = false;
        loop {
            match self.reason_sync().await {
                Ok(()) => failing = false,
                Err(e) if !failing => {
                    log(&format!("listing Reason sessions: {e:#}"));
                    failing = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(EVERY).await;
        }
    }

    /// Keeps `reason network __relay` running. It reads nothing from its stdin, and ends when that closes: with this
    /// daemon, however it ends. Its stderr goes where the daemon's does (the log).
    pub(super) async fn relay_loop(self: Arc<Self>) {
        let mut wait = RELAY_WAIT;
        let mut missing = false;
        loop {
            let started = std::time::Instant::now();
            match agent::find_bin("reason") {
                None if !missing => {
                    log(
                        "reason network __relay: the reason CLI isn't on this daemon's PATH; Reason's cloud agents can't reach this computer's agents",
                    );
                    missing = true;
                }
                None => {}
                Some(bin) => {
                    missing = false;
                    let child = tokio::process::Command::new(&bin)
                        .args(["network", "__relay"])
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn();
                    match child {
                        Ok(mut child) => {
                            // Held here, not left to `wait`, which would close it first.
                            let _stdin = child.stdin.take();
                            match child.wait().await {
                                Ok(status) => log(&format!("reason network __relay stopped ({status})")),
                                Err(e) => log(&format!("reason network __relay: {e}")),
                            }
                        }
                        Err(e) => log(&format!("starting reason network __relay: {e}")),
                    }
                }
            }
            if started.elapsed() > RELAY_MAX_WAIT {
                wait = RELAY_WAIT;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RELAY_MAX_WAIT);
        }
    }

    async fn reason_sync(self: &Arc<Self>) -> Result<()> {
        let Some(bin) = agent::find_bin("reason") else { return Ok(()) };
        let out = tokio::process::Command::new(bin)
            .args(["session", "list", "--limit", "20", "--json"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(30), out).await??;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let list: Value = serde_json::from_slice(&out.stdout)?;
        let live: Vec<&Value> = list["items"]
            .as_array()
            .map(|a| a.iter().filter(|s| s["finished_at"].is_null() && s["is_archived"] != true).collect())
            .unwrap_or_default();

        let now = util::now_ms();
        let mut seen = HashSet::new();
        for s in live {
            let Some(id) = s["session_id"].as_str().or(s["id"].as_str()) else { continue };
            let key = format!("reason:{id}");
            let text = |k: &str| s[k].as_str().filter(|v| !v.is_empty()).map(str::to_string);
            let title = text("title");
            seen.insert(key.clone());
            if !self.inner.lock().unwrap().live.contains_key(&key) {
                let git = GitInfo { root: None, repo: text("repo"), branch: text("branch") };
                // Named for its repo, else for the first words of its title (kept once chosen).
                let name = git
                    .repo
                    .is_none()
                    .then(|| title.as_deref().map(first_words))
                    .flatten()
                    .map(|t| util::joined(&t, "reason"));
                let reg = SessionReg {
                    key: key.clone(),
                    agent: "reason".into(),
                    cwd: "~".into(),
                    agent_session_id: Some(id.to_string()),
                    pid: None,
                    tier: Tier::Push,
                    name,
                };
                self.register(None, reg, git)?;
            }
            let mut g = self.inner.lock().unwrap();
            if let Some(l) = g.live.get_mut(&key) {
                // Seen just now: keeps it from being pruned like a session whose hooks went quiet.
                l.last_hook = Some(now);
                l.doing = title;
                let status = if s["status"].as_str() == Some("running") { "busy" } else { "idle" };
                if l.status.as_ref().is_none_or(|(st, _)| st != status) {
                    l.status = Some((status.to_string(), now));
                }
            }
            // Mail a failed push left behind goes out again.
            let address = g.live.get(&key).map(|l| self.addr(&l.name));
            if let Some(address) = address {
                let waiting: Vec<_> = g.pending(&address)?.into_iter().filter(|m| m.wake).collect();
                if !waiting.is_empty() {
                    self.deliver(&mut g, &key, waiting, true);
                }
            }
        }
        let mut g = self.inner.lock().unwrap();
        let gone: Vec<String> = g
            .live
            .iter()
            .filter(|(k, l)| l.reg.agent == "reason" && !seen.contains(*k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
            }
        }
        Ok(())
    }
}

fn first_words(title: &str) -> String {
    util::slug(&title.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
}
