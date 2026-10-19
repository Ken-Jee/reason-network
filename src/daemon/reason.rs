//! Reason sessions. Reason's agent runs in its cloud, so there is no hook or MCP server here: when `reason` is on
//! (`reason network setup`), the daemon lists your live Reason sessions that are on the network (`reason session list
//! --network --json`: sessions of Reasons you own whose "On the network" switch is on) every few seconds, and delivers
//! to them with `reason session send`. They reply through the Reason Network CLI, which runs here.
//!
//! Only one of your computers lists them, so each session is on the network once: see [`carries`].
//!
//! Where the Reason CLI manages this engine, the daemon also keeps the CLI's `reason network __relay` running: it
//! carries the workspace's cloud agents' calls (Reason's hosted tools) onto the network, for as long as the daemon runs.

use super::{Daemon, log};
use crate::{agent, git::GitInfo, proto::*, util};
use anyhow::{Result, bail};
use iroh::EndpointId;
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
        // On your other computers nothing is listed, so any listed there leave (as below, when they end). A Reason CLI
        // older than `--network` refuses the flag: then the error is logged and nothing is listed.
        let list = if self.carries_reason_sessions() {
            let out = tokio::process::Command::new(bin)
                .args(["session", "list", "--network", "--limit", "20", "--json"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(Duration::from_secs(30), out).await??;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            serde_json::from_slice(&out.stdout)?
        } else {
            Value::Null
        };
        let live = on_network(&list);

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
                let name = Some(name(text("reason_name"), git.repo.as_deref(), title.as_deref()));
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

impl Daemon {
    /// Whether this computer lists your Reason sessions: see [`carries`].
    fn carries_reason_sessions(&self) -> bool {
        let (Some(net), Some(team)) = (self.net(), self.team_state()) else { return true };
        let mine = net
            .remotes()
            .into_iter()
            .filter(|(id, r)| r.online && team.member(id).is_some_and(|m| m.active && m.person == self.person))
            .map(|(id, _)| id);
        carries(self.key.public(), mine)
    }
}

/// Of your computers online now (`mine`, besides this one), the one whose key sorts first lists your Reason sessions,
/// so each is on the network once however many computers you have. Every computer works this out alike from the
/// roster and who it is connected to, with no word between them; when that one goes offline, the next takes over at
/// its next listing. (A Reason session runs in Reason's cloud, on none of them: any one can carry it.)
fn carries(me: EndpointId, mine: impl IntoIterator<Item = EndpointId>) -> bool {
    mine.into_iter().all(|id| me <= id)
}

/// The live sessions the list puts on the network. The server marks each session `on_network` only when it is under a
/// Reason you own whose owner put it on the network; a list without the mark (from a server older than the switch)
/// puts nothing on it, rather than every session you can see, other people's shared Reasons among them.
fn on_network(list: &Value) -> Vec<&Value> {
    list["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s["on_network"] == true && s["finished_at"].is_null() && s["is_archived"] != true)
                .collect()
        })
        .unwrap_or_default()
}

/// Named for its Reason, else its repo, else the first words of its title (kept once chosen).
fn name(reason: Option<String>, repo: Option<&str>, title: Option<&str>) -> String {
    let base = reason
        .or_else(|| repo.and_then(|r| r.rsplit('/').next()).map(str::to_string))
        .or_else(|| title.map(|t| t.split_whitespace().take(3).collect::<Vec<_>>().join(" ")))
        .unwrap_or_default();
    util::joined(&base, "reason")
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;
    use serde_json::json;

    #[test]
    fn only_sessions_the_server_puts_on_the_network_are_listed() {
        let list = json!({ "items": [
            { "session_id": "mine", "on_network": true, "finished_at": null, "is_archived": false },
            { "session_id": "off", "on_network": false, "finished_at": null },
            // From an older server, which doesn't say: another person's shared Reason may be among these.
            { "session_id": "unmarked", "finished_at": null },
            { "session_id": "finished", "on_network": true, "finished_at": "2026-10-19T00:00:00Z" },
            { "session_id": "archived", "on_network": true, "finished_at": null, "is_archived": true },
            { "session_id": "truthy", "on_network": "true", "finished_at": null },
        ]});
        let ids: Vec<&str> = on_network(&list).iter().filter_map(|s| s["session_id"].as_str()).collect();
        assert_eq!(ids, ["mine"]);
        assert!(on_network(&Value::Null).is_empty());
        assert!(on_network(&json!({ "error": "not signed in" })).is_empty());
    }

    #[test]
    fn one_of_your_computers_carries_them() {
        let mut keys: Vec<EndpointId> = (0..3).map(|_| SecretKey::generate().public()).collect();
        keys.sort();
        let (first, second, third) = (keys[0], keys[1], keys[2]);
        // Alone, a computer carries them; with others online, only the one that sorts first does.
        assert!(carries(second, []));
        assert!(carries(first, [second, third]));
        assert!(!carries(second, [first, third]));
        assert!(!carries(third, [second, first]));
        // The first goes offline: the next one takes over.
        assert!(carries(second, [third]));
        // Exactly one of them, whichever view each has of the same three.
        let carriers = keys.iter().filter(|&&me| carries(me, keys.iter().copied().filter(|&k| k != me))).count();
        assert_eq!(carriers, 1);
    }

    #[test]
    fn named_for_the_reason_within_forty_characters() {
        assert_eq!(name(Some("Billing triage".into()), Some("acme/api"), Some("Fix it")), "billing-triage-reason");
        assert_eq!(name(None, Some("acme/api"), Some("Fix flaky billing test")), "api-reason");
        assert_eq!(name(None, None, Some("Fix flaky billing test")), "fix-flaky-billing-reason");
        assert_eq!(name(None, None, None), "reason");
        let long = name(Some("Quarterly revenue reconciliation for every region".into()), None, None);
        assert!(long.len() <= 40 && long.ends_with("-reason"), "{long}");
        assert_eq!(long, "quarterly-revenue-reconciliation-reason");
        let cjk = name(Some("請求書の自動処理とレポート作成のためのワークフロー".into()), None, None);
        assert!(cjk.len() <= 40 && cjk.ends_with("-reason"), "{cjk}");
    }
}
