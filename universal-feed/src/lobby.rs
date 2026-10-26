//! Lobbies: "game servers" for shared live feeds.
//!
//! Members pool what their own platform feeds (e.g. X's algorithm) show them.
//! Every interaction is a vote that immediately re-ranks the pool for every
//! other member: consensus votes, plus extra weight for members who vote like
//! you. The lobby's policy is its algorithmic identity and is portable JSON.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::classify::{classify, is_finance};
use crate::ingest::{clamp_score, prepare_text};
use crate::model::{Candidate, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Impression, Like, Bookmark, Reply, Skip }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Weights { pub impression: f64, pub like: f64, pub bookmark: f64, pub reply: f64, pub skip: f64 }

impl Default for Weights {
    fn default() -> Self { Self { impression: 0.0, like: 1.0, bookmark: 2.0, reply: 1.5, skip: -0.5 } }
}

impl Weights {
    fn of(&self, k: Kind) -> f64 {
        match k {
            Kind::Impression => self.impression, Kind::Like => self.like, Kind::Bookmark => self.bookmark,
            Kind::Reply => self.reply, Kind::Skip => self.skip,
        }
    }
}

/// How a lobby ranks its pool. Missing fields take defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LobbyPolicy {
    /// If non-empty, only posts classified into one of these topics enter the pool.
    pub topics: Vec<String>,
    pub weights: Weights,
    /// Votes lose half their weight every `half_life_secs`.
    pub half_life_secs: f64,
    /// 0 = everyone sees the consensus ranking; 1 = ranked only by members who vote like you.
    pub affinity: f64,
    /// Bonus per ln(number of members whose own feed surfaced the post).
    pub overlap_prior: f64,
    /// Subtracted from posts the viewer has already seen.
    pub seen_penalty: f64,
    /// Further posts by the same author are pushed below the rest.
    pub max_same_author: usize,
    /// Fraction of feed slots (after the first) given to lower-ranked posts so they can collect votes.
    pub exploration: f64,
    /// Finance posts get no vote or overlap amplification unless enabled (pump-and-dump risk).
    pub amplify_finance: bool,
    pub max_members: usize,
    pub max_pool: usize,
}

impl Default for LobbyPolicy {
    fn default() -> Self {
        Self {
            topics: vec![], weights: Weights::default(), half_life_secs: 3600.0, affinity: 0.5,
            overlap_prior: 0.25, seen_penalty: 0.5, max_same_author: 2, exploration: 0.1,
            amplify_finance: false, max_members: 400, max_pool: 2000,
        }
    }
}

impl LobbyPolicy {
    pub fn validate(&self) -> Result<(), String> {
        let w = &self.weights;
        let finite = [w.impression, w.like, w.bookmark, w.reply, w.skip, self.overlap_prior, self.seen_penalty];
        if !finite.iter().all(|x| x.is_finite()) { return Err("weights must be finite".into()); }
        if !(self.half_life_secs.is_finite() && self.half_life_secs > 0.0) { return Err("half_life_secs must be > 0".into()); }
        if !(0.0..=1.0).contains(&self.affinity) { return Err("affinity must be in [0, 1]".into()); }
        if !(0.0..=0.5).contains(&self.exploration) { return Err("exploration must be in [0, 0.5]".into()); }
        if self.max_same_author == 0 || self.max_members == 0 || self.max_pool == 0 {
            return Err("max_same_author, max_members and max_pool must be >= 1".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolItem {
    pub id: Uuid,
    pub source: Source,
    pub external_id: String,
    pub author: String,
    pub text: String,
    pub url: Option<String>,
    pub categories: Vec<String>,
    pub finance: bool,
    /// Engagement on the originating platform, in [0, 1].
    pub source_score: f64,
    pub added_at: DateTime<Utc>,
    /// Members whose feeds surfaced this post. Not exposed to other members.
    #[serde(skip)]
    contributors: HashSet<String>,
}

/// One member's interactions with one post: at most one vote per kind, timestamped
/// when first cast (re-sending doesn't refresh decay).
type Marks = HashMap<Kind, DateTime<Utc>>;

#[derive(Debug, PartialEq)]
pub enum LobbyError { NotMember, NoItem, Full, OwnerCannotLeave, InvalidPolicy(String) }

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct ContributeReport { pub added: usize, pub merged: usize, pub off_topic: usize, pub rejected: usize, pub evicted: usize }

#[derive(Debug, Clone, Serialize)]
pub struct Ranked {
    #[serde(flatten)]
    pub item: PoolItem,
    pub score: f64,
    /// Decayed sum of all members' votes.
    pub global: f64,
    /// Votes weighted by how similarly each voter votes to the viewer.
    pub personal: f64,
    pub contributors: usize,
    pub likes: usize,
    pub bookmarks: usize,
    pub seen: bool,
    /// Placed by exploration rather than score.
    pub explore: bool,
    /// False for finance posts the policy refuses to amplify.
    pub amplified: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LobbyProfile {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub members: usize,
    pub pool: usize,
    pub version: u64,
    pub policy: LobbyPolicy,
}

pub struct Lobby {
    pub id: String,
    pub name: String,
    pub owner: String,
    policy: LobbyPolicy,
    members: Vec<String>,
    version: u64,
    items: HashMap<Uuid, PoolItem>,
    by_key: HashMap<(Source, String), Uuid>,
    votes: HashMap<String, HashMap<Uuid, Marks>>,
}

impl Lobby {
    pub fn new(id: &str, name: &str, owner: &str, policy: LobbyPolicy) -> Result<Self, LobbyError> {
        policy.validate().map_err(LobbyError::InvalidPolicy)?;
        Ok(Self {
            id: id.into(), name: name.into(), owner: owner.into(), policy, members: vec![owner.into()],
            version: 0, items: HashMap::new(), by_key: HashMap::new(), votes: HashMap::new(),
        })
    }

    pub fn version(&self) -> u64 { self.version }
    pub fn is_member(&self, user: &str) -> bool { self.members.iter().any(|m| m == user) }
    fn bump(&mut self) { self.version += 1; }

    pub fn profile(&self) -> LobbyProfile {
        LobbyProfile {
            id: self.id.clone(), name: self.name.clone(), owner: self.owner.clone(), members: self.members.len(),
            pool: self.items.len(), version: self.version, policy: self.policy.clone(),
        }
    }

    pub fn set_policy(&mut self, policy: LobbyPolicy, now: DateTime<Utc>) -> Result<usize, LobbyError> {
        policy.validate().map_err(LobbyError::InvalidPolicy)?;
        self.policy = policy;
        let evicted = self.evict_overflow(now);
        self.bump();
        Ok(evicted)
    }

    /// Ok(true) if newly joined, Ok(false) if already a member.
    pub fn join(&mut self, user: &str) -> Result<bool, LobbyError> {
        if self.is_member(user) { return Ok(false); }
        if self.members.len() >= self.policy.max_members { return Err(LobbyError::Full); }
        self.members.push(user.into());
        self.bump();
        Ok(true)
    }

    /// Leaving removes the member's votes and withdraws posts only they contributed.
    pub fn leave(&mut self, user: &str) -> Result<usize, LobbyError> {
        if user == self.owner { return Err(LobbyError::OwnerCannotLeave); }
        if !self.is_member(user) { return Err(LobbyError::NotMember); }
        self.members.retain(|m| m != user);
        self.votes.remove(user);
        let withdrawn = self.withdraw(user);
        self.bump();
        Ok(withdrawn)
    }

    /// Drop the user as a contributor; remove posts nobody else contributed.
    pub fn withdraw(&mut self, user: &str) -> usize {
        let orphaned: Vec<Uuid> = self.items.values_mut()
            .filter_map(|i| (i.contributors.remove(user) && i.contributors.is_empty()).then_some(i.id))
            .collect();
        for id in &orphaned { self.remove_item(*id); }
        if !orphaned.is_empty() { self.bump(); }
        orphaned.len()
    }

    fn remove_item(&mut self, id: Uuid) {
        if let Some(i) = self.items.remove(&id) { self.by_key.remove(&(i.source, i.external_id)); }
        for v in self.votes.values_mut() { v.remove(&id); }
    }

    /// Add posts from a member's own feed. A post already in the pool gains a contributor,
    /// which is the "several members' algorithms surfaced this" signal.
    pub fn contribute(&mut self, user: &str, candidates: Vec<Candidate>, now: DateTime<Utc>) -> Result<ContributeReport, LobbyError> {
        if !self.is_member(user) { return Err(LobbyError::NotMember); }
        let mut r = ContributeReport::default();
        for c in candidates {
            let Some(text) = prepare_text(&c) else { r.rejected += 1; continue };
            let key = (c.source, c.external_id.clone());
            if let Some(id) = self.by_key.get(&key) {
                if self.items.get_mut(id).expect("by_key is in sync").contributors.insert(user.into()) { r.merged += 1; }
                continue;
            }
            let categories = classify(&text, &|_| true);
            if !self.policy.topics.is_empty() && !categories.iter().any(|c| self.policy.topics.contains(c)) {
                r.off_topic += 1;
                continue;
            }
            let id = Uuid::new_v4();
            self.items.insert(id, PoolItem {
                id, source: c.source, external_id: c.external_id.clone(), author: c.author.clone(),
                finance: is_finance(&text), text, url: c.url.clone(), categories,
                source_score: clamp_score(&c), added_at: now, contributors: HashSet::from([user.to_string()]),
            });
            self.by_key.insert(key, id);
            r.added += 1;
        }
        r.evicted = self.evict_overflow(now);
        if r.added + r.merged + r.evicted > 0 { self.bump(); }
        Ok(r)
    }

    /// Over capacity: evict the lowest-consensus posts, oldest first.
    fn evict_overflow(&mut self, now: DateTime<Utc>) -> usize {
        let over = self.items.len().saturating_sub(self.policy.max_pool);
        if over == 0 { return 0; }
        let mut order: Vec<(f64, DateTime<Utc>, Uuid)> =
            self.items.values().map(|i| (self.consensus(i.id, now), i.added_at, i.id)).collect();
        order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (_, _, id) in order.into_iter().take(over) { self.remove_item(id); }
        over
    }

    /// Record a vote (or retract it with `active = false`). Any interaction counts as seen.
    pub fn interact(&mut self, user: &str, item: Uuid, kind: Kind, active: bool, now: DateTime<Utc>) -> Result<(), LobbyError> {
        if !self.is_member(user) { return Err(LobbyError::NotMember); }
        if !self.items.contains_key(&item) { return Err(LobbyError::NoItem); }
        let marks = self.votes.entry(user.into()).or_default().entry(item).or_default();
        marks.entry(Kind::Impression).or_insert(now);
        if kind != Kind::Impression {
            if active { marks.entry(kind).or_insert(now); } else { marks.remove(&kind); }
        }
        self.bump();
        Ok(())
    }

    fn value(&self, marks: &Marks, now: DateTime<Utc>) -> f64 {
        marks.iter().map(|(k, t)| {
            let age = (now - *t).num_milliseconds().max(0) as f64 / 1000.0;
            self.policy.weights.of(*k) * 0.5f64.powf(age / self.policy.half_life_secs)
        }).sum()
    }

    fn consensus(&self, item: Uuid, now: DateTime<Utc>) -> f64 {
        self.votes.values().filter_map(|v| v.get(&item)).map(|m| self.value(m, now)).sum()
    }

    /// Cosine similarity between the viewer's vote vector and every other member's.
    /// Posts the policy won't amplify are left out, so agreeing on them builds no affinity.
    fn similarities(&self, user: &str, now: DateTime<Utc>) -> HashMap<&str, f64> {
        let amplifiable = |id: &Uuid| self.items.get(id).is_some_and(|i| !i.finance || self.policy.amplify_finance);
        let vector = |v: &HashMap<Uuid, Marks>| -> HashMap<Uuid, f64> {
            v.iter().filter(|(id, _)| amplifiable(id))
                .map(|(id, m)| (*id, self.value(m, now))).filter(|(_, x)| *x != 0.0).collect()
        };
        let norm = |v: &HashMap<Uuid, f64>| v.values().map(|x| x * x).sum::<f64>().sqrt();
        let Some(mine) = self.votes.get(user) else { return HashMap::new() };
        let a = vector(mine);
        let na = norm(&a);
        if na == 0.0 { return HashMap::new(); }
        let mut sims = HashMap::new();
        for (other, theirs) in &self.votes {
            if other == user { continue; }
            let b = vector(theirs);
            let nb = norm(&b);
            if nb == 0.0 { continue; }
            let dot: f64 = a.iter().filter_map(|(id, x)| b.get(id).map(|y| x * y)).sum();
            sims.insert(other.as_str(), dot / (na * nb));
        }
        sims
    }

    /// The pool as `user` should see it right now.
    pub fn rank(&self, user: &str, now: DateTime<Utc>, limit: usize) -> Vec<Ranked> {
        let sims = self.similarities(user, now);
        let alpha = if sims.is_empty() { 0.0 } else { self.policy.affinity };
        let mine = self.votes.get(user);
        let mut rows: Vec<Ranked> = self.items.values().map(|i| {
            let amplified = !i.finance || self.policy.amplify_finance;
            let (mut global, mut personal, mut likes, mut bookmarks) = (0.0, 0.0, 0, 0);
            for (voter, v) in &self.votes {
                let Some(m) = v.get(&i.id) else { continue };
                likes += m.contains_key(&Kind::Like) as usize;
                bookmarks += m.contains_key(&Kind::Bookmark) as usize;
                if !amplified { continue; }
                let val = self.value(m, now);
                global += val;
                if voter != user { personal += sims.get(voter.as_str()).copied().unwrap_or(0.0) * val; }
            }
            let prior = if amplified { self.policy.overlap_prior * (i.contributors.len() as f64).ln() } else { 0.0 };
            let seen = mine.is_some_and(|v| v.contains_key(&i.id));
            let penalty = if seen { self.policy.seen_penalty } else { 0.0 };
            let score = (1.0 - alpha) * global + alpha * personal + prior + 0.1 * i.source_score - penalty;
            Ranked {
                item: i.clone(), score, global, personal, contributors: i.contributors.len(),
                likes, bookmarks, seen, explore: false, amplified,
            }
        }).collect();
        rows.sort_by(|a, b| b.score.total_cmp(&a.score)
            .then(b.item.added_at.cmp(&a.item.added_at))
            .then(a.item.id.cmp(&b.item.id)));
        let rows = self.diversify(rows);
        self.explore(rows, user, limit)
    }

    fn diversify(&self, rows: Vec<Ranked>) -> Vec<Ranked> {
        let mut per_author: HashMap<String, usize> = HashMap::new();
        let (mut out, mut deferred) = (Vec::with_capacity(rows.len()), Vec::new());
        for r in rows {
            let n = per_author.entry(r.item.author.clone()).or_default();
            if *n < self.policy.max_same_author { *n += 1; out.push(r); } else { deferred.push(r); }
        }
        out.extend(deferred);
        out
    }

    /// Deterministically (per viewer and lobby version) swap some slots with posts from below the fold.
    fn explore(&self, mut rows: Vec<Ranked>, user: &str, limit: usize) -> Vec<Ranked> {
        let hash = |salt: &str, slot: usize| {
            let mut h = DefaultHasher::new();
            (user, self.version, slot, salt).hash(&mut h);
            h.finish()
        };
        if self.policy.exploration > 0.0 && rows.len() > limit {
            let tail = rows.len() - limit;
            for slot in 1..limit {
                if (hash("slot", slot) % 10_000) as f64 / 10_000.0 < self.policy.exploration {
                    let pick = limit + (hash("pick", slot) as usize % tail);
                    rows.swap(slot, pick);
                    rows[slot].explore = true;
                }
            }
        }
        rows.truncate(limit);
        rows
    }
}

/// Broadcast to everyone watching a lobby. Voters are not identified.
#[derive(Debug, Clone, Serialize)]
pub struct LobbyEvent {
    pub lobby: String,
    pub version: u64,
    pub kind: String,
    pub item: Option<Uuid>,
}

pub struct LobbyHandle {
    pub id: String,
    pub lobby: Mutex<Lobby>,
    pub events: broadcast::Sender<LobbyEvent>,
}

impl LobbyHandle {
    /// Must not be called while holding `lobby`'s lock for long; it doesn't take it.
    pub fn publish(&self, version: u64, kind: &str, item: Option<Uuid>) {
        // No receivers is fine: nobody is watching.
        let _ = self.events.send(LobbyEvent { lobby: self.id.clone(), version, kind: kind.into(), item });
    }
}

#[derive(Default)]
pub struct Lobbies {
    map: RwLock<HashMap<String, Arc<LobbyHandle>>>,
}

impl Lobbies {
    pub fn get(&self, id: &str) -> Option<Arc<LobbyHandle>> { self.map.read().unwrap().get(id).cloned() }

    pub fn all(&self) -> Vec<Arc<LobbyHandle>> {
        let mut v: Vec<_> = self.map.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// None if a lobby with this id already exists.
    pub fn create(&self, lobby: Lobby) -> Option<Arc<LobbyHandle>> {
        let mut map = self.map.write().unwrap();
        if map.contains_key(&lobby.id) { return None; }
        let id = lobby.id.clone();
        let handle = Arc::new(LobbyHandle { id: id.clone(), lobby: Mutex::new(lobby), events: broadcast::channel(256).0 });
        map.insert(id, handle.clone());
        Some(handle)
    }

    /// Consent withdrawn: pull the user's contributions out of every lobby.
    pub fn withdraw_everywhere(&self, user: &str) -> usize {
        let mut total = 0;
        for h in self.all() {
            let (n, version) = { let mut l = h.lobby.lock().unwrap(); (l.withdraw(user), l.version()) };
            if n > 0 { h.publish(version, "withdrawal", None); }
            total += n;
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t0() -> DateTime<Utc> { DateTime::parse_from_rfc3339("2026-10-26T12:00:00Z").unwrap().into() }

    fn cand(ext: &str, author: &str, text: &str) -> Candidate {
        Candidate { source: Source::X, external_id: ext.into(), author: author.into(), text: text.into(), url: None, score: Some(0.5) }
    }

    fn quiet() -> LobbyPolicy { LobbyPolicy { exploration: 0.0, max_same_author: 99, ..LobbyPolicy::default() } }

    fn lobby_with(policy: LobbyPolicy, members: &[&str]) -> Lobby {
        let mut l = Lobby::new("ai-systems", "AI Systems", "owner", policy).unwrap();
        for m in members { l.join(m).unwrap(); }
        l
    }

    fn id_of(l: &Lobby, ext: &str) -> Uuid { l.by_key[&(Source::X, ext.to_string())] }
    fn order(l: &Lobby, user: &str, now: DateTime<Utc>) -> Vec<String> {
        l.rank(user, now, 50).into_iter().map(|r| r.item.external_id).collect()
    }

    #[test]
    fn one_members_vote_reranks_for_the_next() {
        let mut l = lobby_with(quiet(), &["a", "b"]);
        l.contribute("a", vec![cand("1", "x", "post one"), cand("2", "y", "post two")], t0()).unwrap();
        let before = order(&l, "b", t0());
        let last = before.last().unwrap().clone();
        let v = l.version();
        l.interact("a", id_of(&l, &last), Kind::Like, true, t0()).unwrap();
        assert!(l.version() > v);
        assert_eq!(order(&l, "b", t0())[0], last, "a's like lifts the post for b");
    }

    #[test]
    fn votes_from_like_minded_members_count_more() {
        // c agrees with a on "shared"; a and b split on "pa" vs "pb". c should see "pa" first.
        let mut l = lobby_with(LobbyPolicy { affinity: 1.0, ..quiet() }, &["a", "b", "c"]);
        l.contribute("a", vec![cand("shared", "x", "s"), cand("pa", "y", "a pick"), cand("pb", "z", "b pick")], t0()).unwrap();
        for u in ["a", "c"] { l.interact(u, id_of(&l, "shared"), Kind::Like, true, t0()).unwrap(); }
        l.interact("b", id_of(&l, "shared"), Kind::Skip, true, t0()).unwrap();
        l.interact("a", id_of(&l, "pa"), Kind::Like, true, t0()).unwrap();
        l.interact("b", id_of(&l, "pb"), Kind::Like, true, t0()).unwrap();
        let ranked = l.rank("c", t0(), 50);
        let pos = |e: &str| ranked.iter().position(|r| r.item.external_id == e).unwrap();
        assert!(pos("pa") < pos("pb"));
        // Someone with no votes yet just sees consensus, where pa and pb tie on votes.
        let fresh = l.rank("owner", t0(), 50);
        let g = |e: &str| fresh.iter().find(|r| r.item.external_id == e).unwrap().global;
        assert_eq!(g("pa"), g("pb"));
    }

    #[test]
    fn one_vote_per_kind_and_retraction() {
        let mut l = lobby_with(quiet(), &["a"]);
        l.contribute("a", vec![cand("1", "x", "p")], t0()).unwrap();
        let id = id_of(&l, "1");
        for _ in 0..5 { l.interact("a", id, Kind::Like, true, t0()).unwrap(); }
        assert_eq!(l.rank("owner", t0(), 1)[0].global, 1.0, "spamming likes counts once");
        l.interact("a", id, Kind::Like, false, t0()).unwrap();
        assert_eq!(l.rank("owner", t0(), 1)[0].global, 0.0);
    }

    #[test]
    fn votes_decay_with_half_life() {
        let mut l = lobby_with(quiet(), &["a"]);
        l.contribute("a", vec![cand("1", "x", "p")], t0()).unwrap();
        l.interact("a", id_of(&l, "1"), Kind::Like, true, t0()).unwrap();
        let later = t0() + Duration::seconds(3600);
        assert!((l.rank("owner", later, 1)[0].global - 0.5).abs() < 1e-9);
    }

    #[test]
    fn seen_posts_sink_for_the_viewer_only() {
        let mut l = lobby_with(quiet(), &["a", "b"]);
        l.contribute("a", vec![cand("1", "x", "p1"), cand("2", "y", "p2")], t0()).unwrap();
        let top = order(&l, "a", t0())[0].clone();
        l.interact("a", id_of(&l, &top), Kind::Impression, true, t0()).unwrap();
        assert_ne!(order(&l, "a", t0())[0], top);
        assert_eq!(order(&l, "b", t0())[0], top, "impressions carry no weight for others by default");
    }

    #[test]
    fn overlap_across_members_feeds_is_a_signal() {
        let mut l = lobby_with(quiet(), &["a", "b"]);
        l.contribute("a", vec![cand("solo", "x", "p"), cand("both", "y", "q")], t0()).unwrap();
        let r = l.contribute("b", vec![cand("both", "y", "q")], t0()).unwrap();
        assert_eq!((r.added, r.merged), (0, 1));
        let top = &l.rank("owner", t0(), 1)[0];
        assert_eq!((top.item.external_id.as_str(), top.contributors), ("both", 2));
    }

    #[test]
    fn finance_posts_are_not_amplified_by_default() {
        let mut l = lobby_with(quiet(), &["a", "b", "c"]);
        l.contribute("a", vec![cand("pump", "x", "$XYZ is going to 10x, buy now"), cand("plain", "y", "rust tips")], t0()).unwrap();
        l.contribute("b", vec![cand("pump", "x", "$XYZ is going to 10x, buy now")], t0()).unwrap();
        for u in ["a", "b", "owner"] { l.interact(u, id_of(&l, "pump"), Kind::Like, true, t0()).unwrap(); }
        let ranked = l.rank("c", t0(), 50);
        let pump = ranked.iter().find(|r| r.item.external_id == "pump").unwrap();
        assert!(pump.item.finance && !pump.amplified);
        assert_eq!((pump.global, pump.likes, pump.contributors), (0.0, 3, 2), "votes are counted but not applied");
        assert_eq!(pump.score, 0.1 * 0.5, "only the platform's own engagement score remains");

        let mut l2 = lobby_with(LobbyPolicy { amplify_finance: true, ..quiet() }, &["a"]);
        l2.contribute("a", vec![cand("pump", "x", "$XYZ"), cand("plain", "y", "rust")], t0()).unwrap();
        l2.interact("a", id_of(&l2, "pump"), Kind::Like, true, t0()).unwrap();
        assert_eq!(order(&l2, "owner", t0())[0], "pump", "explicit opt-in restores amplification");
    }

    #[test]
    fn finance_votes_do_not_create_affinity() {
        // c's only vote is on a pump that a also liked. That must not make a's other picks count more for c.
        let mut l = lobby_with(LobbyPolicy { affinity: 1.0, ..quiet() }, &["a", "b", "c"]);
        l.contribute("a", vec![cand("pump", "x", "$XYZ to 10x"), cand("pa", "y", "a pick"), cand("pb", "z", "b pick")], t0()).unwrap();
        for u in ["a", "c"] { l.interact(u, id_of(&l, "pump"), Kind::Like, true, t0()).unwrap(); }
        l.interact("a", id_of(&l, "pa"), Kind::Like, true, t0()).unwrap();
        l.interact("b", id_of(&l, "pb"), Kind::Like, true, t0()).unwrap();
        let ranked = l.rank("c", t0(), 50);
        assert!(ranked.iter().all(|r| r.personal == 0.0), "c has no amplifiable votes, so no personalization");
        let g = |e: &str| ranked.iter().find(|r| r.item.external_id == e).unwrap().score;
        assert_eq!(g("pa"), g("pb"));
    }

    #[test]
    fn diversifies_authors() {
        let mut l = lobby_with(LobbyPolicy { max_same_author: 1, ..quiet() }, &["a"]);
        l.contribute("a", vec![cand("1", "loud", "p1"), cand("2", "loud", "p2"), cand("3", "quiet", "p3")], t0()).unwrap();
        for e in ["1", "2"] { l.interact("a", id_of(&l, e), Kind::Like, true, t0()).unwrap(); }
        let authors: Vec<_> = l.rank("owner", t0(), 3).into_iter().map(|r| r.item.author).collect();
        assert_eq!(authors, vec!["loud", "quiet", "loud"]);
    }

    #[test]
    fn exploration_is_deterministic_and_keeps_top_slot() {
        let mut l = lobby_with(LobbyPolicy { exploration: 0.5, ..quiet() }, &["a"]);
        let posts = (0..40).map(|i| cand(&i.to_string(), &format!("au{i}"), "p")).collect();
        l.contribute("a", posts, t0()).unwrap();
        l.interact("a", id_of(&l, "7"), Kind::Like, true, t0()).unwrap();
        let (r1, r2) = (l.rank("owner", t0(), 10), l.rank("owner", t0(), 10));
        assert_eq!(r1.iter().map(|r| r.item.id).collect::<Vec<_>>(), r2.iter().map(|r| r.item.id).collect::<Vec<_>>());
        assert!(r1.iter().any(|r| r.explore));
        assert!(!r1[0].explore && r1[0].item.external_id == "7");
    }

    #[test]
    fn leaving_withdraws_votes_and_unshared_contributions() {
        let mut l = lobby_with(quiet(), &["a", "b"]);
        l.contribute("a", vec![cand("mine", "x", "p"), cand("ours", "y", "q")], t0()).unwrap();
        l.contribute("b", vec![cand("ours", "y", "q")], t0()).unwrap();
        l.interact("a", id_of(&l, "ours"), Kind::Like, true, t0()).unwrap();
        assert_eq!(l.leave("a"), Ok(1));
        assert_eq!(order(&l, "b", t0()), vec!["ours"]);
        assert_eq!(l.rank("b", t0(), 1)[0].global, 0.0, "a's vote went with them");
        assert_eq!(l.leave("owner"), Err(LobbyError::OwnerCannotLeave));
        assert_eq!(l.contribute("a", vec![], t0()), Err(LobbyError::NotMember));
    }

    #[test]
    fn capacity_limits() {
        let mut l = lobby_with(LobbyPolicy { max_members: 2, max_pool: 2, ..quiet() }, &["a"]);
        assert_eq!(l.join("b"), Err(LobbyError::Full));
        l.contribute("a", vec![cand("1", "x", "p")], t0()).unwrap();
        l.interact("a", id_of(&l, "1"), Kind::Like, true, t0()).unwrap();
        let r = l.contribute("a", vec![cand("2", "y", "p"), cand("3", "z", "p")], t0() + Duration::seconds(1)).unwrap();
        assert_eq!(r.evicted, 1);
        assert!(l.by_key.contains_key(&(Source::X, "1".into())), "voted post survives eviction");
    }

    #[test]
    fn topic_filter_and_policy_validation() {
        let mut l = lobby_with(LobbyPolicy { topics: vec!["rust".into()], ..quiet() }, &["a"]);
        let r = l.contribute("a", vec![cand("1", "x", "tokio tips"), cand("2", "y", "my lunch")], t0()).unwrap();
        assert_eq!((r.added, r.off_topic), (1, 1));
        let bad = LobbyPolicy { affinity: 2.0, ..LobbyPolicy::default() };
        assert!(matches!(Lobby::new("x", "x", "o", bad), Err(LobbyError::InvalidPolicy(_))));
        assert!(matches!(l.set_policy(LobbyPolicy { half_life_secs: 0.0, ..quiet() }, t0()), Err(LobbyError::InvalidPolicy(_))));
    }
}
