//! Team membership: a small replicated state (team metadata, member machines, invites) in which every entry is
//! signed by a member machine's key. Any member can add or remove machines and issue invites; entries merge
//! last-writer-wins by timestamp. Built for a team of friends who trust each other, not adversarial members.

use crate::{
    proof::{self, Proof},
    util,
};
use anyhow::{Result, anyhow, bail};
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use iroh::{EndpointAddr, EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::{IpAddr, SocketAddr},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signed<T> {
    pub body: T,
    pub by: EndpointId,
    pub sig: String,
}

/// What a signature covers: the team, the kind of entry, and the entry itself.
fn payload<T: Serialize>(team: &str, kind: &str, body: &T) -> Vec<u8> {
    let mut p = format!("tincan/1/{team}/{kind}/").into_bytes();
    p.extend(serde_json::to_vec(body).expect("entries serialize"));
    p
}

impl<T: Serialize> Signed<T> {
    pub(crate) fn new(body: T, key: &SecretKey, team: &str, kind: &str) -> Self {
        let sig = key.sign(&payload(team, kind, &body));
        Signed { by: key.public(), sig: HEXLOWER.encode(&sig.to_bytes()), body }
    }

    pub(crate) fn verify(&self, team: &str, kind: &str) -> bool {
        let Ok(bytes) = HEXLOWER.decode(self.sig.as_bytes()) else { return false };
        let Ok(arr) = <[u8; Signature::LENGTH]>::try_from(bytes.as_slice()) else { return false };
        self.by.verify(&payload(team, kind, &self.body), &Signature::from_bytes(&arr)).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    /// A self-hosted iroh relay for the team (`https://relay.example.com`), which is then all its machines use: they
    /// never contact n0. n0's public relays otherwise.
    pub relay: Option<String>,
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub endpoint: EndpointId,
    pub person: String,
    pub machine: String,
    pub active: bool,
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    /// blake3 of the secret; the secret itself only travels in the ticket.
    pub hash: String,
    pub expires: i64,
    /// Set for `reason network invite --me`: only this person can redeem it (adding a machine of their own).
    pub person: Option<String>,
    pub used_by: Option<EndpointId>,
    pub ts: i64,
    /// The team link's invite: anyone with it can join, any number of times, until it is replaced.
    #[serde(default)]
    pub reusable: bool,
}

/// The team link's secret, known to every member so any of them can show the link (`reason network link`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub secret: String,
    pub ts: i64,
}

/// The replicated team state. `admissions` records how each machine got in and is kept forever, so who may
/// sign entries never depends on the order changes arrive in. `members` holds each machine's latest entry
/// (active or removed), last writer wins. A removed machine can't connect or re-admit itself; it isn't made
/// cryptographically powerless (this is for friends, not adversaries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamState {
    pub id: String,
    pub founder: EndpointId,
    pub meta: Signed<Meta>,
    #[serde(default)]
    pub admissions: BTreeMap<String, Signed<Member>>,
    pub members: BTreeMap<String, Signed<Member>>,
    pub invites: BTreeMap<String, Signed<Invite>>,
    #[serde(default)]
    pub link: Option<Signed<Link>>,
    /// Each machine's Reason sign-in proof (a JWS, see [`crate::proof`]), by endpoint id. It signs itself, so it stays
    /// out of the signed entries, and builds that don't know it pass the rest of the state on unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attestations: BTreeMap<String, String>,
}

/// Entries stamped further ahead than this are refused (a broken clock, or worse).
const MAX_SKEW_MS: i64 = 86_400_000;

/// Last-writer-wins order; the signature breaks ties deterministically.
fn newer(ts: i64, sig: &str, than: Option<(i64, &str)>) -> bool {
    than.is_none_or(|(t, s)| (ts, sig) > (t, s))
}

fn next_ts(prev: i64) -> i64 {
    prev.max(util::now_ms() - 1).saturating_add(1)
}

fn plausible(ts: i64) -> bool {
    ts <= util::now_ms().saturating_add(MAX_SKEW_MS)
}

impl TeamState {
    pub fn found(name: &str, person: &str, machine: &str, key: &SecretKey) -> Self {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).expect("system randomness");
        let id = HEXLOWER.encode(&id);
        let now = util::now_ms();
        let meta = Signed::new(Meta { name: name.into(), relay: None, ts: now }, key, &id, "meta");
        let mut state = TeamState {
            id,
            founder: key.public(),
            meta,
            admissions: BTreeMap::new(),
            members: BTreeMap::new(),
            invites: BTreeMap::new(),
            link: None,
            attestations: BTreeMap::new(),
        };
        state.put_member(key, key.public(), person, machine, true);
        state.new_link(key);
        state
    }

    /// The team link's secret, if the team has one.
    pub fn link_secret(&self) -> Option<&str> {
        self.link.as_ref().map(|l| l.body.secret.as_str())
    }

    /// Replaces the team link: the old one stops working.
    pub fn new_link(&mut self, key: &SecretKey) {
        let now = util::now_ms();
        if let Some(old) = self.link_secret().map(hash)
            && let Some(inv) = self.invites.get(&old).map(|i| i.body.clone())
        {
            let expired = Invite { expires: now, ts: next_ts(inv.ts), ..inv };
            self.invites.insert(old, Signed::new(expired, key, &self.id, "invite"));
        }
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).expect("system randomness");
        let secret = HEXLOWER.encode(&raw);
        let body =
            Invite { hash: hash(&secret), expires: i64::MAX / 4, person: None, used_by: None, ts: now, reusable: true };
        self.invites.insert(body.hash.clone(), Signed::new(body, key, &self.id, "invite"));
        let ts = next_ts(self.link.as_ref().map_or(0, |l| l.body.ts));
        self.link = Some(Signed::new(Link { secret, ts }, key, &self.id, "link"));
    }

    /// A copy without the team link, for a machine that was removed.
    /// What a removed machine is shown: who's in the team, without the team link or any invite.
    pub fn without_secrets(&self) -> TeamState {
        TeamState { link: None, invites: BTreeMap::new(), ..self.clone() }
    }

    /// Whether `endpoint` may be an active machine of `person`, on `signer`'s word: one of the person's machines vouches
    /// for it, nobody else has the name, or Reason says it belongs to the account the name is verified for.
    fn may_take(&self, person: &str, endpoint: &EndpointId, signer: &EndpointId) -> bool {
        self.active().any(|m| m.endpoint == *signer && m.person == person)
            || !self.active().any(|m| m.person == person && m.endpoint != *endpoint)
            || self
                .proof(endpoint)
                .is_some_and(|p| p.name == person && self.verified_people().get(person) == Some(&p.sub))
    }

    /// The machine's sign-in proof, if it has one that verifies now.
    pub fn proof(&self, endpoint: &EndpointId) -> Option<Proof> {
        proof::verify(self.attestations.get(&endpoint.to_string())?, endpoint).ok()
    }

    /// Verified people: each name an active machine's proof is for, with the Reason account it belongs to.
    pub fn verified_people(&self) -> HashMap<String, String> {
        self.active()
            .filter_map(|m| self.proof(&m.endpoint).filter(|p| p.name == m.person))
            .map(|p| (p.name, p.sub))
            .collect()
    }

    /// Records `endpoint`'s sign-in proof, if it verifies and is newer than the one it has; true if it was taken.
    pub fn attest(&mut self, endpoint: &EndpointId, jws: &str) -> bool {
        let Ok(p) = proof::verify(jws, endpoint) else { return false };
        if self.proof(endpoint).is_some_and(|cur| cur.iat >= p.iat) {
            return false;
        }
        self.attestations.insert(endpoint.to_string(), jws.trim().to_string());
        true
    }

    pub fn name(&self) -> &str {
        &self.meta.body.name
    }

    pub fn member(&self, id: &EndpointId) -> Option<&Member> {
        self.members.get(&id.to_string()).map(|m| &m.body)
    }

    /// Machines that were ever admitted: the founder, then anyone admitted by an admitted machine.
    fn admitted(&self) -> HashSet<EndpointId> {
        self.admission_depths().into_keys().collect()
    }

    /// Each admitted machine, with how many admissions it is from the founder.
    fn admission_depths(&self) -> HashMap<EndpointId, usize> {
        let mut depths = HashMap::new();
        loop {
            let before = depths.len();
            for a in self.admissions.values() {
                let e = a.body.endpoint;
                let depth = if a.by == e { (e == self.founder).then_some(0) } else { depths.get(&a.by).map(|d| d + 1) };
                if let Some(d) = depth.filter(|_| !depths.contains_key(&e)) {
                    depths.insert(e, d);
                }
            }
            if depths.len() == before {
                return depths;
            }
        }
    }

    pub fn is_active(&self, id: &EndpointId) -> bool {
        self.member(id).is_some_and(|m| m.active) && self.admitted().contains(id)
    }

    pub fn active(&self) -> impl Iterator<Item = &Member> {
        let admitted = self.admitted();
        self.members.values().map(|m| &m.body).filter(move |m| m.active && admitted.contains(&m.endpoint))
    }

    pub fn people(&self) -> Vec<String> {
        let mut p: Vec<String> = self.active().map(|m| m.person.clone()).collect();
        p.sort();
        p.dedup();
        p
    }

    pub fn put_member(&mut self, key: &SecretKey, endpoint: EndpointId, person: &str, machine: &str, active: bool) {
        let ts = next_ts(self.member(&endpoint).map_or(0, |m| m.ts));
        let body = Member { endpoint, person: person.into(), machine: machine.into(), active, ts };
        let entry = Signed::new(body, key, &self.id, "member");
        if active && !self.admissions.contains_key(&endpoint.to_string()) {
            self.admissions.insert(endpoint.to_string(), entry.clone());
        }
        self.members.insert(endpoint.to_string(), entry);
    }

    pub fn set_relay(&mut self, key: &SecretKey, relay: Option<String>) {
        let body = Meta { name: self.meta.body.name.clone(), relay, ts: next_ts(self.meta.body.ts) };
        self.meta = Signed::new(body, key, &self.id, "meta");
    }

    /// Creates an invite for `secret` (an invite code's, or random for a ticket).
    pub fn invite(&mut self, key: &SecretKey, secret: &str, hours: u64, person: Option<String>) {
        let now = util::now_ms();
        let body = Invite {
            hash: hash(secret),
            expires: now + hours as i64 * 3_600_000,
            person,
            used_by: None,
            ts: now,
            reusable: false,
        };
        self.invites.insert(body.hash.clone(), Signed::new(body, key, &self.id, "invite"));
    }

    pub fn invite_unused(&self, secret: &str) -> bool {
        self.invites.get(&hash(secret)).is_some_and(|i| i.body.used_by.is_none() && i.body.expires > util::now_ms())
    }

    /// Checks that `secret` admits `person` (with the joiner's verified sign-in proof, if any), and marks the invite
    /// used by `joiner`.
    pub fn redeem(
        &mut self,
        key: &SecretKey,
        secret: &str,
        person: &str,
        joiner: EndpointId,
        proof: Option<&Proof>,
    ) -> Result<()> {
        let h = hash(secret);
        let signed = self.invites.get(&h).ok_or_else(|| anyhow!("unknown invite"))?;
        let (inv, signer) = (signed.body.clone(), signed.by);
        if !self.is_active(&signer) {
            bail!("this invite was made by a machine that has left the team; ask for a new one");
        }
        match inv.used_by {
            // A retry by a machine that got in with it (its first reply was lost) is fine; anyone else isn't.
            Some(u) if u == joiner && self.is_active(&joiner) => return Ok(()),
            Some(_) => bail!("this invite was already used; ask for a new one"),
            None => {}
        }
        if inv.expires <= util::now_ms() {
            bail!(if inv.reusable {
                "this team link was replaced; ask for the new one"
            } else {
                "this invite expired; ask for a new one"
            });
        }
        match &inv.person {
            Some(p) if p != person => bail!("this invite is for {p}'s own machines"),
            // Only a person's own machines can add another of theirs (see `may_take`).
            Some(p) if self.member(&key.public()).is_none_or(|m| &m.person != p) => {
                bail!("only one of {p}'s machines can let this one in; try again while one is online")
            }
            Some(_) => {}
            // Another computer of the account the name is verified for may join under it.
            None if proof.is_some_and(|p| p.name == person && self.verified_people().get(person) == Some(&p.sub)) => {}
            None if self.active().any(|m| m.person == person && m.endpoint != joiner) => {
                bail!("'{person}' is already in this team; join with another name (--as)")
            }
            None => {}
        }
        if !inv.reusable {
            let used = Invite { used_by: Some(joiner), ts: next_ts(inv.ts), ..inv };
            self.invites.insert(h, Signed::new(used, key, &self.id, "invite"));
        }
        Ok(())
    }

    /// Merges another copy of the state; true if anything changed.
    pub fn merge(&mut self, other: &TeamState) -> Result<bool> {
        if other.id != self.id || other.founder != self.founder {
            bail!("state from a different team");
        }
        let mut changed = false;
        // Proofs check themselves; they come before members, whose names they can vouch for (`may_take`).
        for (k, jws) in &other.attestations {
            if let Ok(id) = k.parse::<EndpointId>()
                && self.attestations.get(k) != Some(jws)
                && self.attest(&id, jws)
            {
                changed = true;
            }
        }
        // Then admissions: they decide who may sign everything else. One admission can vouch for the signer of
        // another, so repeat until nothing new lands.
        loop {
            let admitted = self.admitted();
            let mut added = false;
            for (k, a) in &other.admissions {
                if self.admissions.contains_key(k)
                    || *k != a.body.endpoint.to_string()
                    || !a.body.active
                    || !plausible(a.body.ts)
                {
                    continue;
                }
                let vouched = if a.by == a.body.endpoint { a.by == self.founder } else { admitted.contains(&a.by) };
                if vouched && a.verify(&self.id, "member") {
                    self.admissions.insert(k.clone(), a.clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
            changed = true;
        }
        let depths = self.admission_depths();
        let admitted: HashSet<EndpointId> = depths.keys().copied().collect();
        // Machines before the ones they let in, so a person's first machine holds their name before another of theirs
        // is checked against it (`may_take`), whatever order the keys sort in.
        let mut entries: Vec<_> = other.members.iter().collect();
        entries.sort_by_key(|(_, e)| depths.get(&e.body.endpoint).copied().unwrap_or(usize::MAX));
        for (k, e) in entries {
            let cur = self.members.get(k);
            if *k != e.body.endpoint.to_string()
                || !plausible(e.body.ts)
                || !newer(e.body.ts, &e.sig, cur.map(|c| (c.body.ts, c.sig.as_str())))
            {
                continue;
            }
            // A machine may sign its own departure, never its own return (the founder's very first entry aside).
            let self_admit = e.by == e.body.endpoint && e.body.active;
            if self_admit && !(e.by == self.founder && cur.is_none()) {
                continue;
            }
            // A person's name belongs to their machines: a machine can't take a name another active machine has,
            // unless a machine of that person vouches for it (adding another of their own machines).
            if e.body.active && !self.may_take(&e.body.person, &e.body.endpoint, &e.by) {
                continue;
            }
            if admitted.contains(&e.by) && e.verify(&self.id, "member") {
                self.members.insert(k.clone(), e.clone());
                changed = true;
            }
        }
        let m = &other.meta;
        if newer(m.body.ts, &m.sig, Some((self.meta.body.ts, &self.meta.sig)))
            && plausible(m.body.ts)
            && admitted.contains(&m.by)
            && m.verify(&self.id, "meta")
        {
            self.meta = m.clone();
            changed = true;
        }
        if let Some(l) = &other.link
            && newer(l.body.ts, &l.sig, self.link.as_ref().map(|c| (c.body.ts, c.sig.as_str())))
            && plausible(l.body.ts)
            && admitted.contains(&l.by)
            && l.verify(&self.id, "link")
        {
            self.link = Some(l.clone());
            changed = true;
        }
        for (h, inv) in &other.invites {
            let cur = self.invites.get(h);
            if &inv.body.hash == h
                && plausible(inv.body.ts)
                && newer(inv.body.ts, &inv.sig, cur.map(|c| (c.body.ts, c.sig.as_str())))
                && admitted.contains(&inv.by)
                && inv.verify(&self.id, "invite")
            {
                self.invites.insert(h.clone(), inv.clone());
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Validates a state received from a stranger (the join reply): every entry must trace back to the founder.
    pub fn verified(other: &TeamState) -> Result<TeamState> {
        let mut s = TeamState {
            id: other.id.clone(),
            founder: other.founder,
            meta: other.meta.clone(),
            admissions: BTreeMap::new(),
            members: BTreeMap::new(),
            invites: BTreeMap::new(),
            link: None,
            attestations: BTreeMap::new(),
        };
        s.merge(other)?;
        let admitted = s.admitted();
        if !admitted.contains(&s.founder) {
            bail!("the team's founding entry does not verify");
        }
        if !admitted.contains(&s.meta.by) || !s.meta.verify(&s.id, "meta") {
            bail!("team metadata does not verify");
        }
        Ok(s)
    }

    /// Drops invites that expired more than a week ago.
    pub fn prune_invites(&mut self) {
        let cutoff = util::now_ms() - 7 * 86_400_000;
        self.invites.retain(|_, i| i.body.expires > cutoff);
    }
}

pub fn hash(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// Everything a new machine needs to join: the team, an invite secret, and some members to ask (by key; their
/// addresses are looked up). Packed small, since it ends up in a link.
#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub team: String,
    pub name: String,
    pub secret: String,
    pub relay: Option<String>,
    /// How the first peer (the machine that made the ticket) was reachable then: its home relay and addresses.
    /// Hints for a fast start; the keys are looked up for current addresses anyway.
    pub hint_relay: Option<String>,
    pub hint_ips: Vec<SocketAddr>,
    pub peers: Vec<EndpointId>,
}

/// What a ticket starts with. The payload after it is shared with every peer (and earlier builds): never change it.
const TICKET_PREFIX: &str = "rn1-";
/// What earlier builds wrote: still read.
const OLD_TICKET_PREFIX: &str = "tincan1";
/// In place of a length: an n0 relay, given by its region (`use1-1` for `https://use1-1.relay.n0.iroh.link./`).
const N0_RELAY: u8 = 0xFF;
const N0_RELAY_SUFFIX: &str = ".relay.n0.iroh.link./";

/// A length byte, then the text (cut to 250 bytes, so a length never reads as a marker).
fn put_text(b: &mut Vec<u8>, text: &str) {
    let mut n = text.len().min(250);
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    b.push(n as u8);
    b.extend(&text.as_bytes()[..n]);
}

fn n0_relay(url: &str) -> Option<&str> {
    let host = url.strip_prefix("https://")?.trim_end_matches('/');
    let region = host.strip_suffix(".relay.n0.iroh.link.").or_else(|| host.strip_suffix(".relay.n0.iroh.link"))?;
    (!region.is_empty() && region.len() < 32 && region.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
        .then_some(region)
}

impl Ticket {
    /// `rn1-` + base64url of: team id (16 bytes), secret (16), name, team relay, first peer's relay hint (each
    /// length-prefixed, 0 for none), its address hints (count, then family 4/6, address, port), then 32 bytes per
    /// member key.
    pub fn encode(&self) -> String {
        let mut b = Vec::new();
        b.extend(HEXLOWER.decode(self.team.as_bytes()).expect("team ids are hex"));
        b.extend(HEXLOWER.decode(self.secret.as_bytes()).expect("secrets are hex"));
        let name: String = self.name.chars().take(60).collect();
        put_text(&mut b, &name);
        for relay in [&self.relay, &self.hint_relay] {
            let relay = relay.as_deref().unwrap_or("");
            match n0_relay(relay) {
                // One of n0's relays: just its region, which keeps links short.
                Some(region) => {
                    b.push(N0_RELAY);
                    put_text(&mut b, region);
                }
                None => put_text(&mut b, relay),
            }
        }
        let ips: Vec<&SocketAddr> = self.hint_ips.iter().take(2).collect();
        b.push(ips.len() as u8);
        for a in ips {
            match a.ip() {
                IpAddr::V4(ip) => {
                    b.push(4);
                    b.extend(ip.octets());
                }
                IpAddr::V6(ip) => {
                    b.push(6);
                    b.extend(ip.octets());
                }
            }
            b.extend(a.port().to_be_bytes());
        }
        for p in &self.peers {
            b.extend(p.as_bytes());
        }
        format!("{TICKET_PREFIX}{}", BASE64URL_NOPAD.encode(&b))
    }

    /// Whether `text` holds a team link or ticket (it may not be a whole one).
    pub fn is_ticket(text: &str) -> bool {
        text.contains(TICKET_PREFIX) || text.contains(OLD_TICKET_PREFIX)
    }

    pub fn decode(s: &str) -> Result<Self> {
        let s = s.trim().rsplit('#').next().unwrap_or_default();
        let data = s
            .strip_prefix(TICKET_PREFIX)
            .or_else(|| s.strip_prefix(OLD_TICKET_PREFIX))
            .ok_or_else(|| anyhow!("not a Reason Network invite"))?;
        let b =
            BASE64URL_NOPAD.decode(data.as_bytes()).map_err(|_| anyhow!("this invite is damaged (was it cut off?)"))?;
        let damaged = || anyhow!("this invite is damaged (was it cut off?)");
        let mut at = 0;
        let mut take = |n: usize| -> Result<&[u8]> {
            let part = b.get(at..at + n).ok_or_else(damaged)?;
            at += n;
            Ok(part)
        };
        let team = HEXLOWER.encode(take(16)?);
        let secret = HEXLOWER.encode(take(16)?);
        let mut text = || -> Result<String> {
            let n = take(1)?[0];
            if n == N0_RELAY {
                let n = take(1)?[0] as usize;
                return Ok(format!("https://{}{N0_RELAY_SUFFIX}", String::from_utf8_lossy(take(n)?)));
            }
            Ok(String::from_utf8_lossy(take(n as usize)?).into_owned())
        };
        let name = text()?;
        let relay = Some(text()?).filter(|r| !r.is_empty());
        let hint_relay = Some(text()?).filter(|r| !r.is_empty());
        let mut hint_ips = Vec::new();
        for _ in 0..take(1)?[0] {
            let ip = match take(1)?[0] {
                4 => IpAddr::from(<[u8; 4]>::try_from(take(4)?).expect("4 bytes")),
                6 => IpAddr::from(<[u8; 16]>::try_from(take(16)?).expect("16 bytes")),
                _ => return Err(damaged()),
            };
            let port = u16::from_be_bytes(take(2)?.try_into().expect("2 bytes"));
            hint_ips.push(SocketAddr::new(ip, port));
        }
        let mut peers = Vec::new();
        while let Ok(k) = take(32) {
            peers.push(EndpointId::from_bytes(k.try_into().expect("32 bytes")).map_err(|_| damaged())?);
        }
        // A key cut short means the link was.
        if peers.is_empty() || at != b.len() {
            return Err(damaged());
        }
        Ok(Ticket { team, name, secret, relay, hint_relay, hint_ips, peers })
    }

    /// The peers to dial, the first with its hints. With the team's own relay, all of them are found there.
    pub fn addrs(&self) -> Vec<EndpointAddr> {
        self.peers
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut a = EndpointAddr::new(*id);
                let relay = self.relay.as_deref().or(self.hint_relay.as_deref().filter(|_| i == 0));
                if let Some(url) = relay.and_then(|u| u.parse().ok()) {
                    a = a.with_relay_url(url);
                }
                if i == 0 {
                    for ip in &self.hint_ips {
                        a = a.with_ip_addr(*ip);
                    }
                }
                a
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretKey {
        SecretKey::generate()
    }

    #[test]
    fn an_approvals_signature_covers_every_field_and_the_team() {
        let k = key();
        let approval = crate::proto::Approval {
            about: "01REQ".into(),
            asker: "alice/api".into(),
            approver: "bob".into(),
            approved: true,
            question: "Rename the billing table?".into(),
            request: "please rename billing".into(),
            note: "ok".into(),
            at: 1,
        };
        let signed = Signed::new(approval, &k, "acme", "approval");
        assert!(signed.verify("acme", "approval"));
        assert!(!signed.verify("other", "approval"));
        assert!(!signed.verify("acme", "member"));
        for change in [
            |a: &mut crate::proto::Approval| a.about = "01OTHER".into(),
            |a: &mut crate::proto::Approval| a.asker = "mallory/api".into(),
            |a: &mut crate::proto::Approval| a.approved = false,
            |a: &mut crate::proto::Approval| a.request = "drop the table".into(),
        ] {
            let mut forged = signed.clone();
            change(&mut forged.body);
            assert!(!forged.verify("acme", "approval"));
        }
        let mut wrong = signed.clone();
        wrong.by = key().public();
        assert!(!wrong.verify("acme", "approval"));
    }

    fn random_secret() -> String {
        crate::invite::secret(&crate::invite::generate())
    }

    #[test]
    fn found_invite_join_merge() {
        let (a, b, c) = (key(), key(), key());
        let mut sa = TeamState::found("vibes", "alex", "mbp", &a);
        let secret = random_secret();
        sa.invite(&a, &secret, 24, None);

        // b joins through a: a redeems and adds b; b verifies what it receives.
        sa.redeem(&a, &secret, "alice", b.public(), None).unwrap();
        sa.put_member(&a, b.public(), "alice", "xps", true);
        let mut sb = TeamState::verified(&sa).unwrap();
        assert_eq!(sb, sa);
        assert!(sb.is_active(&b.public()));
        assert!(sa.redeem(&a, &secret, "eve", c.public(), None).is_err(), "invites are single use");

        // b invites c; a learns about c by merging b's copy.
        let s2 = random_secret();
        sb.invite(&b, &s2, 1, None);
        sb.redeem(&b, &s2, "sam", c.public(), None).unwrap();
        sb.put_member(&b, c.public(), "sam", "nuc", true);
        assert!(sa.merge(&sb).unwrap());
        assert!(sa.is_active(&c.public()));
        assert!(!sa.merge(&sb).unwrap(), "merging again changes nothing");
        assert_eq!(sa.people(), vec!["alex", "alice", "sam"]);
    }

    #[test]
    fn rejects_forgery_and_self_readd() {
        let (a, b, mallory) = (key(), key(), key());
        let mut sa = TeamState::found("vibes", "alex", "mbp", &a);
        sa.put_member(&a, b.public(), "alice", "xps", true);

        // A stranger cannot add itself.
        let mut forged = sa.clone();
        forged.put_member(&mallory, mallory.public(), "mallory", "evil", true);
        assert!(!sa.merge(&forged).unwrap());
        assert!(!sa.is_active(&mallory.public()));

        // A tampered entry fails verification.
        let mut tampered = sa.clone();
        let e = tampered.members.get_mut(&b.public().to_string()).unwrap();
        e.body.person = "root".into();
        e.body.ts += 10;
        assert!(!sa.merge(&tampered).unwrap());

        // b may leave on its own, but not come back on its own.
        let mut leaving = sa.clone();
        leaving.put_member(&b, b.public(), "alice", "xps", false);
        assert!(sa.merge(&leaving).unwrap());
        assert!(!sa.is_active(&b.public()));
        sa.put_member(&a, b.public(), "alice", "xps", true);

        // Once removed, b cannot sign itself back in.
        sa.put_member(&a, b.public(), "alice", "xps", false);
        let mut sb = sa.clone();
        sb.put_member(&b, b.public(), "alice", "xps", true);
        assert!(!sa.merge(&sb).unwrap());
        assert!(!sa.is_active(&b.public()));
    }

    #[test]
    fn founder_leaving_doesnt_lock_the_team() {
        let (f, b, d) = (key(), key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &f);
        s.put_member(&f, b.public(), "alice", "xps", true);
        s.put_member(&f, f.public(), "alex", "mbp", false); // founder leaves
        let secret = random_secret();
        s.invite(&b, &secret, 24, None);
        s.redeem(&b, &secret, "sam", d.public(), None).unwrap();
        s.put_member(&b, d.public(), "sam", "nuc", true);
        let sd = TeamState::verified(&s).unwrap();
        assert!(sd.is_active(&d.public()) && sd.is_active(&b.public()) && !sd.is_active(&f.public()));
    }

    #[test]
    fn merges_converge_whatever_the_order() {
        let (f, b, c, d) = (key(), key(), key(), key());
        let mut base = TeamState::found("vibes", "alex", "mbp", &f);
        base.put_member(&f, b.public(), "alice", "xps", true);
        // Concurrently: f removes b while b adds c.
        let mut at_f = base.clone();
        at_f.put_member(&f, b.public(), "alice", "xps", false);
        let mut at_b = base.clone();
        at_b.put_member(&b, c.public(), "sam", "nuc", true);
        let mut x = at_f.clone();
        x.merge(&at_b).unwrap();
        let mut y = at_b.clone();
        y.merge(&at_f).unwrap();
        assert_eq!(x, y);
        assert!(x.is_active(&c.public()) && !x.is_active(&b.public()));
        // A newcomer sees the same, including c (admitted by b, who is gone now).
        let secret = random_secret();
        x.invite(&f, &secret, 24, None);
        x.redeem(&f, &secret, "dee", d.public(), None).unwrap();
        x.put_member(&f, d.public(), "dee", "pi", true);
        let z = TeamState::verified(&x).unwrap();
        assert!(z.is_active(&c.public()) && !z.is_active(&b.public()));
    }

    #[test]
    fn a_persons_machines_join_whatever_their_keys_sort() {
        // f lets in b and b lets in c, all alex's: a newcomer keeps all three, in any key order.
        let keys = [key(), key(), key()];
        for (i, j, k) in [(0, 1, 2), (0, 2, 1), (1, 0, 2), (1, 2, 0), (2, 0, 1), (2, 1, 0)] {
            let (f, b, c) = (&keys[i], &keys[j], &keys[k]);
            let mut s = TeamState::found("vibes", "alex", "mbp", f);
            s.put_member(f, b.public(), "alex", "mini", true);
            s.put_member(b, c.public(), "alex", "nuc", true);
            let v = TeamState::verified(&s).unwrap();
            assert!([f, b, c].iter().all(|m| v.is_active(&m.public())), "keys in order {i}{j}{k}");
        }
    }

    #[test]
    fn invites_die_with_their_maker_and_cant_readmit() {
        let (a, b, c) = (key(), key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        let first = random_secret();
        s.invite(&a, &first, 24, None);
        s.redeem(&a, &first, "alice", b.public(), None).unwrap();
        s.put_member(&a, b.public(), "alice", "xps", true);
        assert!(s.redeem(&a, &first, "alice", b.public(), None).is_ok(), "a retry by the same machine is fine");
        let from_b = random_secret();
        s.invite(&b, &from_b, 720, None);
        s.put_member(&a, b.public(), "alice", "xps", false);
        assert!(s.redeem(&a, &first, "alice", b.public(), None).is_err(), "removed: the old invite doesn't readmit");
        assert!(s.redeem(&a, &from_b, "sam", c.public(), None).is_err(), "invites by a removed machine are dead");
    }

    #[test]
    fn refuses_far_future_entries() {
        let (a, b) = (key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        let mut evil = s.clone();
        let body = Member { endpoint: b.public(), person: "x".into(), machine: "y".into(), active: true, ts: i64::MAX };
        let e = Signed::new(body, &a, &evil.id, "member");
        evil.admissions.insert(b.public().to_string(), e.clone());
        evil.members.insert(b.public().to_string(), e);
        assert!(!s.merge(&evil).unwrap());
        s.put_member(&a, b.public(), "x", "y", true); // and no overflow on the next change
    }

    #[test]
    fn names_are_unique_unless_invited_for_self() {
        let (a, b) = (key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        let open = random_secret();
        s.invite(&a, &open, 24, None);
        assert!(s.redeem(&a, &open, "alex", b.public(), None).is_err());
        let mine = random_secret();
        s.invite(&a, &mine, 24, Some("alex".into()));
        assert!(s.redeem(&a, &mine, "alice", b.public(), None).is_err());
        s.redeem(&a, &mine, "alex", b.public(), None).unwrap();
    }

    #[test]
    fn a_teammate_cannot_take_someone_elses_name() {
        let (a, b, fake, mine) = (key(), key(), key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        let link = s.link_secret().unwrap().to_string();
        s.redeem(&a, &link, "alice", b.public(), None).unwrap();
        s.put_member(&a, b.public(), "alice", "xps", true);
        // alice's machine signs a new machine in as alex, on its copy, and sends it over.
        let mut evil = s.clone();
        evil.put_member(&b, fake.public(), "alex", "evil", true);
        s.merge(&evil).unwrap();
        assert!(!s.is_active(&fake.public()), "alice's machine made itself a second alex");
        assert_eq!(s.active().filter(|m| m.person == "alex").count(), 1);
        // alex adding another of alex's machines is fine.
        let mut own = s.clone();
        own.put_member(&a, mine.public(), "alex", "mini", true);
        s.merge(&own).unwrap();
        assert!(s.is_active(&mine.public()));
    }

    #[test]
    fn ticket_roundtrip() {
        let (a, b) = (key(), key());
        let t = Ticket {
            team: "00112233445566778899aabbccddeeff".into(),
            name: "the boys".into(),
            secret: random_secret(),
            relay: Some("https://relay.example.com".into()),
            hint_relay: Some("https://euc1-1.relay.n0.iroh.link./".into()),
            hint_ips: vec!["192.168.1.20:51234".parse().unwrap(), "[fe80::1]:51234".parse().unwrap()],
            peers: vec![a.public(), b.public()],
        };
        let enc = t.encode();
        assert!(enc.len() < 300, "short enough for a link: {}", enc.len());
        assert_eq!(t.addrs()[0].ip_addrs().count(), 2);
        // Every peer through the team's own relay, never the n0 relay the hint names.
        for a in t.addrs() {
            assert_eq!(a.relay_urls().map(|u| u.to_string()).collect::<Vec<_>>(), ["https://relay.example.com/"]);
        }
        assert_eq!(Ticket::decode(&format!("  {enc}\n")).unwrap(), t);
        assert_eq!(Ticket::decode(&format!("https://example.com/join/#{enc}")).unwrap(), t, "a whole link works too");
        assert!(Ticket::decode(&enc[..enc.len() - 10]).is_err());
        assert!(Ticket::decode("tincan1!!!").is_err());
        assert!(Ticket::decode("rn1-!!!").is_err());
        assert!(Ticket::decode("hello").is_err());
    }

    #[test]
    fn links_name_n0_relays_by_region_and_old_links_still_work() {
        let a = key();
        let mut t = Ticket {
            team: "00112233445566778899aabbccddeeff".into(),
            name: "vibes".into(),
            secret: random_secret(),
            relay: None,
            hint_relay: Some("https://use1-1.relay.n0.iroh.link./".into()),
            hint_ips: vec![],
            peers: vec![a.public()],
        };
        let short = t.encode();
        assert_eq!(Ticket::decode(&short).unwrap(), t);
        // The same link as 0.5 wrote it, with the relay spelled out.
        let mut b = HEXLOWER.decode(t.team.as_bytes()).unwrap();
        b.extend(HEXLOWER.decode(t.secret.as_bytes()).unwrap());
        for f in ["vibes", "", "https://use1-1.relay.n0.iroh.link./"] {
            put_text(&mut b, f);
        }
        b.push(0);
        b.extend(a.public().as_bytes());
        let old = format!("{OLD_TICKET_PREFIX}{}", BASE64URL_NOPAD.encode(&b));
        assert!(short.len() + 30 < old.len(), "{} vs {}", short.len(), old.len());
        assert_eq!(Ticket::decode(&old).unwrap(), t);
        t.hint_relay = Some("https://relay.example.com/".into());
        assert_eq!(Ticket::decode(&t.encode()).unwrap(), t);
    }

    #[test]
    fn team_link_is_reusable_until_replaced() {
        let (a, b, c, d) = (key(), key(), key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        let link = s.link_secret().unwrap().to_string();
        s.redeem(&a, &link, "alice", b.public(), None).unwrap();
        s.put_member(&a, b.public(), "alice", "xps", true);
        s.redeem(&a, &link, "sam", c.public(), None).unwrap();
        s.put_member(&a, c.public(), "sam", "nuc", true);
        assert!(s.redeem(&a, &link, "alice", d.public(), None).is_err(), "names stay unique");
        // A joiner learns the link, and can show it too.
        let joined = TeamState::verified(&s).unwrap();
        assert_eq!(joined.link_secret(), Some(link.as_str()));
        assert_eq!(s.without_secrets().link, None);
        assert!(s.without_secrets().invites.is_empty());
        s.new_link(&b);
        assert!(s.redeem(&a, &link, "dee", d.public(), None).is_err(), "the old link stops working");
        let fresh = s.link_secret().unwrap().to_string();
        s.redeem(&a, &fresh, "dee", d.public(), None).unwrap();
    }

    #[test]
    fn links_are_rn1_and_tincan1_links_still_work() {
        let t = Ticket {
            team: "00112233445566778899aabbccddeeff".into(),
            name: "vibes".into(),
            secret: random_secret(),
            relay: None,
            hint_relay: None,
            hint_ips: vec![],
            peers: vec![key().public()],
        };
        let enc = t.encode();
        let payload = enc.strip_prefix("rn1-").expect("emitted as rn1-");
        for link in [
            enc.clone(),
            format!("https://reasonmachines.com/network/join/#{enc}"),
            format!("tincan1{payload}"),
            format!("https://reasonmachines.com/network/join/#tincan1{payload}"),
        ] {
            assert_eq!(Ticket::decode(&link).unwrap(), t, "{link}");
        }
        assert!(Ticket::decode(&format!("rn2-{payload}")).is_err());
    }

    fn proof(sub: &str, name: &str, k: &SecretKey) -> String {
        crate::proof::tests::proof(sub, name, &k.public())
    }

    #[test]
    fn proofs_mark_people_verified_and_travel_with_the_state() {
        let (a, b) = (key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        assert!(s.attest(&a.public(), &proof("u_alex", "alex", &a)));
        s.put_member(&a, b.public(), "alice", "xps", true);
        // A proof for another machine, or a forged one, is refused.
        assert!(!s.attest(&b.public(), &proof("u_alice", "alice", &a)));
        let forged = crate::proof::tests::sign_with(
            &key(),
            serde_json::json!({"alg": "EdDSA", "kid": crate::proof::tests::KID, "typ": "reason-network-proof"}),
            crate::proof::tests::claims("u_alice", "alice", &b.public()),
        );
        assert!(!s.attest(&b.public(), &forged));
        assert_eq!(s.verified_people().into_iter().collect::<Vec<_>>(), [("alex".into(), "u_alex".into())]);
        // A proof for a name the machine doesn't go by doesn't verify the person.
        let mut other = s.clone();
        other.attestations.insert(b.public().to_string(), proof("u_x", "bob", &b));
        assert!(s.merge(&other).unwrap());
        assert!(!s.verified_people().contains_key("alice") && !s.verified_people().contains_key("bob"));
        // Peers drop proofs that don't verify.
        let mut bad = s.clone();
        bad.attestations.insert(a.public().to_string(), forged.clone());
        assert!(!s.merge(&bad).unwrap());
        assert_eq!(s.verified_people().get("alex").map(String::as_str), Some("u_alex"));
        // A joiner gets them with the state.
        let joined = TeamState::verified(&s).unwrap();
        assert_eq!(joined.attestations, s.attestations);
    }

    #[test]
    fn state_from_builds_without_proofs_merges_both_ways() {
        let (a, b) = (key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        // Without proofs the state serializes as before them.
        assert!(!serde_json::to_string(&s).unwrap().contains("attestations"));
        s.attest(&a.public(), &proof("u_alex", "alex", &a));
        // An older build reads the state, drops what it doesn't know, signs a change, and sends it back.
        let mut json: serde_json::Value = serde_json::to_value(&s).unwrap();
        json.as_object_mut().unwrap().remove("attestations");
        let mut old: TeamState = serde_json::from_value(json).unwrap();
        assert!(old.attestations.is_empty());
        old.put_member(&a, b.public(), "alice", "xps", true);
        assert!(s.merge(&old).unwrap());
        assert!(
            s.is_active(&b.public()) && s.verified_people().contains_key("alex"),
            "the change lands, the proof stays"
        );
        // Our state, with proofs, verifies for it too: no signed entry changed.
        assert!(TeamState::verified(&s).is_ok());
    }

    #[test]
    fn a_verified_name_is_its_accounts() {
        let (a, b, c, d) = (key(), key(), key(), key());
        let mut s = TeamState::found("vibes", "alex", "mbp", &a);
        s.attest(&a.public(), &proof("u_alex", "alex", &a));
        let link = s.link_secret().unwrap().to_string();
        let p = |sub: &str, name: &str, k: &SecretKey| crate::proof::verify(&proof(sub, name, k), &k.public()).unwrap();
        // Another account, or no proof, can't take it.
        let err = s.redeem(&a, &link, "alex", b.public(), Some(&p("u_mallory", "alex", &b))).unwrap_err();
        assert!(err.to_string().contains("already in this team"), "{err}");
        assert!(s.redeem(&a, &link, "alex", b.public(), None).is_err());
        // Another computer of the same account can, through the team link, let in by anyone.
        s.redeem(&a, &link, "alice", c.public(), None).unwrap();
        s.put_member(&a, c.public(), "alice", "xps", true);
        s.redeem(&c, &link, "alex", d.public(), Some(&p("u_alex", "alex", &d))).unwrap();
        s.attest(&d.public(), &proof("u_alex", "alex", &d));
        s.put_member(&c, d.public(), "alex", "mini", true);
        let peer = TeamState::verified(&s).unwrap();
        assert!(peer.is_active(&d.public()), "alice's machine let in alex's second computer");
        // Without its proof, peers don't take alice's machine's word that it is alex's.
        let mut forged = s.clone();
        forged.attestations.remove(&d.public().to_string());
        assert!(!TeamState::verified(&forged).unwrap().is_active(&d.public()));
        // A name nobody verified stays with whoever has it, proof or not.
        let e = key();
        let err = s.redeem(&a, &link, "alice", e.public(), Some(&p("u_alice", "alice", &e))).unwrap_err();
        assert!(err.to_string().contains("already in this team"), "{err}");
    }
}
