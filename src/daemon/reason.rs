//! Reason sessions. Reason's agent runs in its cloud, so there is no hook or MCP server here: when `reason` is on
//! (`reason network setup`), the daemon lists your live Reason sessions that are on the network (`reason session list
//! --network --json`: sessions of Reasons you own whose "On the network" switch is on) every few seconds, and delivers
//! to them with `reason session send`. They reply through the Reason Network CLI, which runs here.
//!
//! Only one of your computers lists them, so each session is on the network once: see [`listing`]. A session that
//! leaves the list (or every one, when this computer stops listing or can't list) leaves the network at once and is
//! forgotten here with the mail it never read.
//!
//! Where the Reason CLI manages this engine, the daemon also keeps the CLI's `reason network __relay` running: it
//! carries the workspace's cloud agents' calls (Reason's hosted tools) onto the network, for as long as the daemon runs.

use super::{Daemon, log};
use crate::{agent, git::GitInfo, proto::*, team::TeamState, util};
use anyhow::{Result, bail};
use iroh::EndpointId;
use serde_json::Value;
use std::{collections::HashSet, sync::Arc, time::Duration};

const EVERY: Duration = Duration::from_secs(15);
/// Listing failures in a row after which the sessions listed here leave the network: one is often a blip (a timeout, a
/// token refresh) and costs the sessions and their mail nothing; two in a row (30 s) means this computer can't list
/// them now (signed out, the API refusing), and a session must not stay reachable after its Reason left the network.
const FAILURES: u32 = 2;
/// The relay is started again this long after it stops, doubling up to `RELAY_MAX_WAIT` while it keeps stopping.
const RELAY_WAIT: Duration = Duration::from_secs(30);
const RELAY_MAX_WAIT: Duration = Duration::from_secs(600);

impl Daemon {
    pub(super) async fn reason_loop(self: Arc<Self>) {
        let mut round = Round::default();
        // Just started, this computer doesn't yet see which of your others list your Reason sessions: with any others,
        // it waits a round for their presence, so a restart doesn't put each session on the network twice.
        if self.others_of_mine() {
            tokio::time::sleep(EVERY).await;
        }
        loop {
            match self.reason_sync(&mut round).await {
                Ok(()) => round.failures = 0,
                Err(e) => {
                    round.failures += 1;
                    if round.failures == 1 {
                        log(&format!("listing Reason sessions: {e:#}"));
                    }
                    if round.failures >= FAILURES {
                        round.listed.clear();
                        self.drop_reason_sessions(&HashSet::new());
                    }
                }
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

    async fn reason_sync(self: &Arc<Self>, round: &mut Round) -> Result<()> {
        let which = self.reason_listing();
        // Another of your computers lists them, or this one can't: any listed here leave, so the next one takes over.
        let bin = agent::find_bin("reason").filter(|_| which != Listing::None);
        let Some(bin) = bin else {
            round.listed.clear();
            self.drop_reason_sessions(&HashSet::new());
            return Ok(());
        };
        // A Reason CLI older than `--network` refuses the flag: a failure like any other (see FAILURES).
        let out = tokio::process::Command::new(bin)
            .args(["session", "list", "--network", "--limit", "20", "--json"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(30), out).await??;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let list: Value = serde_json::from_slice(&out.stdout)?;
        let id = |s: &Value| s["session_id"].as_str().or(s["id"].as_str()).map(str::to_string);
        let live = on_network(&list);
        let listed: HashSet<String> = live.iter().filter_map(|s| id(s)).collect();
        // Behind a computer of yours that could list them but shows none, a session goes on from here once it has been
        // listed twice: by then that computer has put it on the network if it can.
        let live: Vec<&Value> = match which {
            Listing::Seen => live.into_iter().filter(|s| id(s).is_some_and(|i| round.listed.contains(&i))).collect(),
            _ => live,
        };
        round.listed = listed;

        let now = util::now_ms();
        let mut seen = HashSet::new();
        for s in live {
            let Some(id) = id(s) else { continue };
            let id = id.as_str();
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
        self.drop_reason_sessions(&seen);
        Ok(())
    }

    /// Takes the Reason sessions here that aren't in `keep` off the network now, forgetting each with the mail it never
    /// read: it left because its Reason left the network, ended, or another of your computers lists it, and its name
    /// must not hold mail here for a day.
    fn drop_reason_sessions(&self, keep: &HashSet<String>) {
        let mut g = self.inner.lock().unwrap();
        let gone: Vec<String> = g
            .live
            .iter()
            .filter(|(k, l)| l.reg.agent == "reason" && !keep.contains(*k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &gone {
            if let Some(l) = g.live.remove(key) {
                let _ = g.store.forget_session(&l.name, &self.person);
            }
        }
        drop(g);
        if !gone.is_empty() {
            self.presence_changed();
        }
    }
}

/// What the Reason listing keeps from one round to the next.
#[derive(Default)]
struct Round {
    /// Listing failures in a row.
    failures: u32,
    /// The session ids the last listing returned.
    listed: HashSet<String>,
}

impl Daemon {
    /// How this computer lists your Reason sessions this round: see [`listing`].
    fn reason_listing(&self) -> Listing {
        let (Some(net), Some(team)) = (self.net(), self.team_state()) else { return Listing::All };
        let mine = net
            .remotes()
            .into_iter()
            .filter(|(id, r)| r.online && team.member(id).is_some_and(|m| m.active && m.person == self.person))
            .map(|(id, r)| (id, r.presence.sessions.iter().any(|s| s.agent == "reason")));
        listing(self.key.public(), mine)
    }

    fn others_of_mine(&self) -> bool {
        self.team_state().is_some_and(|t| others_of_mine(&t, &self.person, self.key.public()))
    }
}

/// Whether the roster has another active computer of `person`'s besides `me`.
fn others_of_mine(team: &TeamState, person: &str, me: EndpointId) -> bool {
    team.active().any(|m| m.person == person && m.endpoint != me)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// Another computer of yours lists them: none here.
    None,
    /// Every session listed.
    All,
    /// Only sessions the last listing returned too.
    Seen,
}

/// Your Reason sessions are listed by the first of your computers online now (`mine`, besides this one, each with
/// whether it shows a Reason session) that can list them, in key order: so each is on the network once however many
/// computers you have. A computer with a smaller key that shows a Reason session lists them, so this one lists none.
/// One that shows none either has none on the network or can't list them (no Reason CLI, signed out, a CLI too old):
/// this one lists, but puts a session on only once it has been listed twice ([`Listing::Seen`]), giving that computer
/// a round to show it. Every computer works this out alike from the roster and the presence it already gets; when the
/// first stops showing them (offline, or it can no longer list), the next takes over within a round or two. (A Reason
/// session runs in Reason's cloud, on none of them: any one can carry it.) Two computers can still show the same
/// session for a round while they find out; the later one then drops it.
fn listing(me: EndpointId, mine: impl IntoIterator<Item = (EndpointId, bool)>) -> Listing {
    let before: Vec<bool> = mine.into_iter().filter(|(id, _)| *id < me).map(|(_, shows)| shows).collect();
    if before.iter().any(|shows| *shows) {
        Listing::None
    } else if before.is_empty() {
        Listing::All
    } else {
        Listing::Seen
    }
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
    fn the_first_of_your_computers_that_shows_them_lists_them() {
        let mut keys: Vec<EndpointId> = (0..3).map(|_| SecretKey::generate().public()).collect();
        keys.sort();
        let (first, second, third) = (keys[0], keys[1], keys[2]);
        // Alone, or first in key order, a computer lists them all.
        assert_eq!(listing(second, []), Listing::All);
        assert_eq!(listing(first, [(second, true), (third, true)]), Listing::All);
        // Behind one that shows a Reason session, it lists none.
        assert_eq!(listing(second, [(first, true), (third, false)]), Listing::None);
        assert_eq!(listing(third, [(first, true), (second, false)]), Listing::None);
        // Behind ones that show none (none on the network, or they can't list), it lists, a session at its second
        // listing; so the first that can't list (no CLI, signed out) doesn't leave them off the network.
        assert_eq!(listing(second, [(first, false), (third, true)]), Listing::Seen);
        assert_eq!(listing(third, [(first, false), (second, false)]), Listing::Seen);
        // The first goes offline: the next one takes over.
        assert_eq!(listing(second, [(third, false)]), Listing::All);
        // Once the first shows them, exactly one of the three lists them.
        let shows = |k: EndpointId| k == first;
        let listers = keys
            .iter()
            .filter(|&&me| {
                listing(me, keys.iter().copied().filter(|&k| k != me).map(|k| (k, shows(k)))) != Listing::None
            })
            .count();
        assert_eq!(listers, 1);
    }

    #[test]
    fn a_restart_waits_a_round_only_beside_other_computers_of_yours() {
        let (me, other, theirs) = (SecretKey::generate(), SecretKey::generate(), SecretKey::generate());
        let mut team = TeamState::found("acme", "alex", "laptop", &me);
        // Alone, it lists at once; a teammate's computer changes nothing.
        assert!(!others_of_mine(&team, "alex", me.public()));
        team.put_member(&me, theirs.public(), "sam", "desk", true);
        assert!(!others_of_mine(&team, "alex", me.public()));
        // Another computer of yours may list them already: it waits for that one's presence first.
        team.put_member(&me, other.public(), "alex", "desktop", true);
        assert!(others_of_mine(&team, "alex", me.public()));
        team.put_member(&me, other.public(), "alex", "desktop", false);
        assert!(!others_of_mine(&team, "alex", me.public()));
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
