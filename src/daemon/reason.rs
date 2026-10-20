//! Reason sessions. Reason's agent runs in its cloud, so there is no hook or MCP server here: when `reason` is on
//! (`reason network setup`), the daemon lists your live Reason sessions that are on the network (`reason session list
//! --network --json`: sessions of Reasons you own whose "On the network" switch is on) every few seconds, and delivers
//! to them with `reason session send`. They reply through the Reason Network CLI, which runs here.
//!
//! Only one of your computers lists them, so each session is on the network once: see [`listing`]. A session a listing
//! leaves out (its Reason left the network, or it ended) leaves the network and is forgotten here with the mail it never
//! read. When this computer can't list them or another takes over, they leave the network too, but each keeps its name
//! and unread mail here until a listing shows it again (or the day after which any session is forgotten).
//!
//! Where the Reason CLI manages this engine, the daemon also keeps the CLI's `reason network __relay` running: it
//! carries the workspace's cloud agents' calls (Reason's hosted tools) onto the network, for as long as the daemon runs.

use super::{Daemon, log};
use crate::{agent, git::GitInfo, net::Remote, proto::*, team::TeamState, util};
use anyhow::{Result, bail};
use iroh::EndpointId;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

const EVERY: Duration = Duration::from_secs(15);
/// Listing failures in a row after which the sessions listed here leave the network: one is often a blip (a timeout, a
/// token refresh); two in a row (30 s) means this computer can't list them now (signed out, the API refusing), and a
/// session must not stay reachable after its Reason may have left the network. They keep their mail (see the top).
const FAILURES: u32 = 2;
/// How long a restarted daemon looks for another computer of yours to come online before listing.
const LINKS_WAIT: Duration = Duration::from_secs(5);
/// The relay is started again this long after it stops, doubling up to `RELAY_MAX_WAIT` while it keeps stopping.
const RELAY_WAIT: Duration = Duration::from_secs(30);
const RELAY_MAX_WAIT: Duration = Duration::from_secs(600);

impl Daemon {
    pub(super) async fn reason_loop(self: Arc<Self>) {
        let mut round = Round::default();
        // Just started, this computer doesn't yet see which of your others lists your Reason sessions. With another of
        // yours online, it waits a round for that one's presence, so a restart doesn't put each session on the network
        // twice; alone (or with the others offline) it lists at once. Links take a moment to come up, so it looks for
        // them a few seconds first, where the roster has others of yours.
        if self.others_of_mine() {
            let until = tokio::time::Instant::now() + LINKS_WAIT;
            while !self.others_online() && tokio::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if self.others_online() {
                tokio::time::sleep(EVERY).await;
            }
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
                        self.drop_reason_sessions(&HashSet::new(), None);
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
            self.drop_reason_sessions(&HashSet::new(), None);
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
        let listing_ok = listed.clone();
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
        self.drop_reason_sessions(&seen, Some(&listing_ok));
        self.forget_held(&listing_ok);
        Ok(())
    }

    /// After a listing (`listed`, the ids it returned): forgets the Reason sessions held here, off the network since
    /// listing failed or another computer took over, that it left out (their Reason left the network, or they ended),
    /// with the mail they never read, so they no longer show offline or take mail for a day.
    fn forget_held(&self, listed: &HashSet<String>) {
        let g = self.inner.lock().unwrap();
        let held = g.store.sessions_of("reason").unwrap_or_default();
        let gone: Vec<String> = held
            .into_iter()
            .filter(|r| !g.live.contains_key(&r.key))
            .filter(|r| r.key.strip_prefix("reason:").is_some_and(|id| forgets(Some(listed), id)))
            .map(|r| r.name)
            .collect();
        for name in &gone {
            let _ = g.store.forget_session(name, &self.person);
        }
        drop(g);
        if !gone.is_empty() {
            self.presence_changed();
        }
    }

    /// Takes the Reason sessions here that aren't in `keep` off the network now. With the ids a listing just returned
    /// (`listed`), one it left out is forgotten with the mail it never read ([`forgets`]); any other keeps its name and
    /// mail here, for when a listing shows it again.
    fn drop_reason_sessions(&self, keep: &HashSet<String>, listed: Option<&HashSet<String>>) {
        let now = util::now_ms();
        let mut g = self.inner.lock().unwrap();
        let gone: Vec<String> = g
            .live
            .iter()
            .filter(|(k, l)| l.reg.agent == "reason" && !keep.contains(*k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &gone {
            if let Some(l) = g.live.remove(key) {
                let id = l.reg.agent_session_id.as_deref().unwrap_or_default();
                let _ = if forgets(listed, id) {
                    g.store.forget_session(&l.name, &self.person)
                } else {
                    g.store.touch_session(&l.name, now)
                };
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

/// Whether a Reason session leaving the network here is forgotten with its unread mail: only when a listing succeeded
/// (`listed`, the ids it returned) and left it out, which is its Reason leaving the network or it ending. Taken off
/// because this computer can't list now, or because another took over, it keeps both: senders were told their mail
/// arrived, and it goes in when a listing shows the session again.
fn forgets(listed: Option<&HashSet<String>>, id: &str) -> bool {
    listed.is_some_and(|listed| !listed.contains(id))
}

impl Daemon {
    /// How this computer lists your Reason sessions this round: see [`listing`].
    fn reason_listing(&self) -> Listing {
        let (Some(net), Some(team)) = (self.net(), self.team_state()) else { return Listing::All };
        let shows = self.inner.lock().unwrap().live.values().any(|l| l.reg.agent == "reason");
        let mine = net
            .remotes()
            .into_iter()
            .filter(|(id, r)| r.online && team.member(id).is_some_and(|m| m.active && m.person == self.person))
            .map(|(id, r)| (id, r.presence.sessions.iter().any(|s| s.agent == "reason")));
        listing(self.key.public(), shows, mine)
    }

    fn others_of_mine(&self) -> bool {
        self.team_state().is_some_and(|t| others_of_mine(&t, &self.person, self.key.public()))
    }

    fn others_online(&self) -> bool {
        let (Some(net), Some(team)) = (self.net(), self.team_state()) else { return false };
        others_online(&net.remotes(), &team, &self.person)
    }
}

/// Whether another active computer of `person`'s is online now: a restart waits a round for its presence.
fn others_online(remotes: &HashMap<EndpointId, Remote>, team: &TeamState, person: &str) -> bool {
    remotes.iter().any(|(id, r)| r.online && team.member(id).is_some_and(|m| m.active && m.person == person))
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

/// Which of your computers lists your Reason sessions, so each is on the network once however many computers you have.
/// The one already showing them keeps listing them (`shows`, and for the others online now, `mine`): a computer that
/// shows none lists none while another shows them, so the sessions don't move, or change names, each time a computer
/// comes back. Only two that both show them (for the round they find out) settle it by key order. With none showing
/// them, the first in key order lists them all; one that sorts later lists too, but puts a session on only once it has
/// been listed twice ([`Listing::Seen`]), giving the earlier one a round to show it. A computer that can't list them
/// (no Reason CLI, signed out, a CLI too old) shows none, so the next takes over within a round or two. Every computer
/// works this out alike from the roster and the presence it already gets. (A Reason session runs in Reason's cloud, on
/// none of them: any one can carry it.)
fn listing(me: EndpointId, shows: bool, mine: impl IntoIterator<Item = (EndpointId, bool)>) -> Listing {
    let mine: Vec<(EndpointId, bool)> = mine.into_iter().collect();
    if mine.iter().any(|(id, other)| *other && (!shows || *id < me)) {
        Listing::None
    } else if shows || mine.iter().all(|(id, _)| me < *id) {
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
    fn the_computer_already_listing_them_keeps_them() {
        let mut keys: Vec<EndpointId> = (0..3).map(|_| SecretKey::generate().public()).collect();
        keys.sort();
        let (first, second, third) = (keys[0], keys[1], keys[2]);
        // With none showing them: alone or first in key order, a computer lists them all; later ones list a session
        // once it has been listed twice, so the first that can list them gets a round to show it.
        assert_eq!(listing(second, false, []), Listing::All);
        assert_eq!(listing(first, false, [(second, false), (third, false)]), Listing::All);
        assert_eq!(listing(third, false, [(first, false), (second, false)]), Listing::Seen);
        // One shows them (it took over while the first couldn't list): it keeps them, whatever its key, and the first,
        // back or able again, lists none. So the sessions don't move, or change names, each time a computer comes back.
        assert_eq!(listing(first, false, [(second, true), (third, false)]), Listing::None);
        assert_eq!(listing(third, false, [(first, false), (second, true)]), Listing::None);
        assert_eq!(listing(second, true, [(first, false), (third, false)]), Listing::All);
        // Two that both show them (the round they find out) settle it by key order.
        assert_eq!(listing(first, true, [(second, true)]), Listing::All);
        assert_eq!(listing(second, true, [(first, true)]), Listing::None);
        // The one showing them goes offline (or stops, unable to list): the others take over.
        assert_eq!(listing(first, false, [(third, false)]), Listing::All);
        // Whichever shows them, exactly one of the three lists them.
        for lister in [first, second, third] {
            let listers = keys
                .iter()
                .filter(|&&me| {
                    let others = keys.iter().copied().filter(|&k| k != me).map(|k| (k, k == lister));
                    listing(me, me == lister, others) != Listing::None
                })
                .count();
            assert_eq!(listers, 1);
        }
    }

    #[test]
    fn only_a_listing_that_leaves_a_session_out_forgets_it_and_its_mail() {
        let listed: HashSet<String> = ["still-on".to_string()].into();
        // Its Reason left the network, or it ended: forgotten with its mail.
        assert!(forgets(Some(&listed), "switched-off"));
        assert!(!forgets(Some(&listed), "still-on"));
        // Listing failed, or another computer took over: it keeps its name and the mail senders were told arrived.
        assert!(!forgets(None, "switched-off"));
    }

    #[test]
    fn a_restart_waits_a_round_only_while_another_computer_of_yours_is_online() {
        let (me, other, theirs) = (SecretKey::generate(), SecretKey::generate(), SecretKey::generate());
        let mut team = TeamState::found("acme", "alex", "laptop", &me);
        team.put_member(&me, other.public(), "alex", "desktop", true);
        team.put_member(&me, theirs.public(), "sam", "desk", true);
        let remote = |online: bool| Remote { presence: Default::default(), last_seen: 0, online };
        // Your other computer is in the roster but offline: no one else lists them, so no wait.
        assert!(!others_online(&HashMap::from([(other.public(), remote(false))]), &team, "alex"));
        // A teammate's computer online changes nothing.
        assert!(!others_online(&HashMap::from([(theirs.public(), remote(true))]), &team, "alex"));
        assert!(others_online(&HashMap::from([(other.public(), remote(true))]), &team, "alex"));
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
