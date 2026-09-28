//! Bounded completion history and generation watermarks for `agentbus wait`.
//!
//! This projection is deliberately separate from the verbose event log. It is
//! atomically replaced, small enough to read on every wait poll, and carries a
//! monotone generation so a waiter can take its watermark before resolving a
//! session without racing a fast turn.

use crate::event::{self, Event, Kind};
use serde_json::{json, Value};
use std::path::Path;

/// Bumped only when a field is renamed, removed or changes meaning, the rule
/// `event::SNAPSHOT_VERSION` follows. `pruned_epoch` arrived under 1 without
/// one: it is additive, an older reader ignores it, and a file without it
/// still loads (see `legacy_pruned_epoch`).
const VERSION: u64 = 1;
const MAX_RESULT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_age_secs: u64,
    pub max_records: usize,
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_age_secs: 7 * 24 * 60 * 60,
            max_records: 10_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Index {
    pub generation: u64,
    /// Earliest generation that has not been discarded. A request below
    /// `floor - 1` has crossed retained history and must fail explicitly.
    pub floor: u64,
    /// Newest timestamp (epoch seconds, as `record_epoch` reads it) of any
    /// record ever discarded, or `None` if no record with a timestamp has
    /// been. Monotone, like `floor`, and its counterpart for `newest_since`:
    /// records are ordered by generation, not time, so a discarded record can
    /// be newer than every retained one and only this can say so.
    pub pruned_epoch: Option<u64>,
    records: Vec<Value>,
    limits: Limits,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(Value),
    Pending,
    Expired { floor: u64 },
}

impl Default for Index {
    fn default() -> Self {
        Self {
            generation: 0,
            floor: 1,
            pruned_epoch: None,
            records: Vec::new(),
            limits: Limits::default(),
        }
    }
}

impl Index {
    #[cfg(test)]
    fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        Self::decode(&text)
    }

    /// The inverse of `encode`. Limits are not persisted, so these are always
    /// the defaults; a caller with others reapplies them.
    fn decode(text: &str) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        if value.get("version").and_then(Value::as_u64) != Some(VERSION) {
            return Self::default();
        }
        let generation = value.get("generation").and_then(Value::as_u64).unwrap_or(0);
        let floor = value
            .get("floor")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let records = value
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pruned_epoch = match value.get("pruned_epoch") {
            Some(field) => field.as_u64(),
            None => legacy_pruned_epoch(floor, &records),
        };
        Self {
            generation,
            floor,
            pruned_epoch,
            records,
            limits: Limits::default(),
        }
    }

    pub fn push_events(&mut self, events: &[Event], now: u64) -> bool {
        let mut changed = false;
        for event in events {
            if !matches!(event.kind, Kind::TurnEnd { .. }) {
                continue;
            }
            self.generation = self.generation.saturating_add(1);
            let mut record = event.to_json();
            record
                .as_object_mut()
                .expect("event JSON is an object")
                .insert("generation".into(), json!(self.generation));
            if record_epoch(&record).is_none() {
                record
                    .as_object_mut()
                    .expect("event JSON is an object")
                    .insert("retained_at".into(), json!(now));
            }
            clamp_result(&mut record);
            self.records.push(record);
            changed = true;
        }
        if changed {
            self.prune(now);
        }
        changed
    }

    pub fn first_after(&self, session: &str, generation: u64) -> Lookup {
        if generation.saturating_add(1) < self.floor {
            return Lookup::Expired { floor: self.floor };
        }
        self.records
            .iter()
            .find(|record| {
                record.get("session").and_then(Value::as_str) == Some(session)
                    && record
                        .get("generation")
                        .and_then(Value::as_u64)
                        .is_some_and(|candidate| candidate > generation)
            })
            .cloned()
            .map(Lookup::Found)
            .unwrap_or(Lookup::Pending)
    }

    pub fn newest_since(&self, session: &str, since: u64) -> Lookup {
        let found = self.records.iter().rev().find(|record| {
            record.get("session").and_then(Value::as_str) == Some(session)
                && record_epoch(record).is_some_and(|timestamp| timestamp >= since)
        });
        if let Some(record) = found {
            return Lookup::Found(record.clone());
        }
        // Not "since is older than the oldest retained record": retention is
        // by generation, so the first retained record need not be the oldest
        // by time, and a discarded answer newer than `since` can sit behind it.
        if self.pruned_epoch.is_some_and(|pruned| since <= pruned) {
            Lookup::Expired { floor: self.floor }
        } else {
            Lookup::Pending
        }
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(&json!({
            "version": VERSION,
            "generation": self.generation,
            "floor": self.floor,
            "pruned_epoch": self.pruned_epoch,
            "records": self.records,
        }))
        .unwrap_or_default()
    }

    pub fn write_atomic(&self, path: &Path) -> bool {
        let Some(dir) = path.parent() else {
            return false;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.encode()).is_ok() && std::fs::rename(tmp, path).is_ok()
    }

    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(self.limits.max_age_secs);
        while self
            .records
            .first()
            .is_some_and(|record| record_epoch(record).is_some_and(|timestamp| timestamp < cutoff))
        {
            self.remove_oldest();
        }
        while self.records.len() > self.limits.max_records {
            self.remove_oldest();
        }
        while self.records.len() > 1 && self.encode().len() > self.limits.max_bytes {
            self.remove_oldest();
        }
    }

    fn remove_oldest(&mut self) {
        if self.records.is_empty() {
            return;
        }
        let removed = self.records.remove(0);
        if let Some(generation) = removed.get("generation").and_then(Value::as_u64) {
            self.floor = self.floor.max(generation.saturating_add(1));
        }
        // A record with no timestamp at all is left out, and that is exact
        // rather than lax: `newest_since` can never match such a record while
        // it is retained, so discarding it cannot hide an answer. It only
        // arises from a loaded file, since `push_events` stamps `retained_at`
        // on anything whose `ts` does not parse.
        if let Some(epoch) = record_epoch(&removed) {
            self.pruned_epoch = Some(self.pruned_epoch.map_or(epoch, |seen| seen.max(epoch)));
        }
    }
}

/// `pruned_epoch` for a file written before the field existed.
///
/// With `floor == 1` nothing was ever discarded, so `None` is exact. Past
/// that, the discarded timestamps are unknowable, and the only sound value is
/// "anything", `u64::MAX` — but the watermark never decreases, so that would
/// turn every later `newest_since` miss into `Expired` for good, including
/// the ordinary `wait --since` issued before the turn it names has ended.
/// Instead take the newest retained timestamp. Every discarded record precedes
/// every retained one in generation, so this covers each of them unless its
/// timestamp is newer than the whole retained window, and it is strictly more
/// conservative than the rule it replaces, which trusted the *first* retained
/// record's. With nothing retained there is nothing to bound by; `None` keeps
/// the old answer, `Pending`, for that case.
fn legacy_pruned_epoch(floor: u64, records: &[Value]) -> Option<u64> {
    if floor <= 1 {
        return None;
    }
    records.iter().filter_map(record_epoch).max()
}

fn record_epoch(record: &Value) -> Option<u64> {
    record
        .get("ts")
        .and_then(Value::as_str)
        .and_then(event::iso_to_epoch)
        .or_else(|| record.get("retained_at").and_then(Value::as_u64))
}

fn clamp_result(record: &mut Value) {
    let Some(object) = record.as_object_mut() else {
        return;
    };
    let Some(full) = object.get("result_full").and_then(Value::as_str) else {
        return;
    };
    if full.len() <= MAX_RESULT_BYTES {
        return;
    }
    let mut end = MAX_RESULT_BYTES;
    while !full.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = full[..end].to_string();
    truncated.push_str("\n[agentbus: completion truncated]");
    object.insert("result_full".into(), json!(truncated));
    object.insert("result_truncated".into(), json!(true));
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds past 12:00:00, carried into the minutes: iso_to_epoch rejects
    /// a seconds field of 60 or more.
    fn stamp(second: u64) -> String {
        format!("2026-08-09T12:{:02}:{:02}Z", second / 60, second % 60)
    }

    fn completion(session: &str, second: u64) -> Event {
        Event {
            ts: stamp(second),
            source: "test",
            session: session.into(),
            kind: Kind::TurnEnd {
                duration_ms: Some(1),
                result: Some(format!("answer {second}")),
                result_full: Some(format!("answer {second}")),
            },
        }
    }

    #[test]
    fn first_after_returns_the_first_new_matching_completion() {
        let now = event::iso_to_epoch("2026-08-09T12:01:00Z").unwrap();
        let mut index = Index::default();
        index.push_events(&[completion("a", 1), completion("b", 2)], now);
        let watermark = index.generation;
        index.push_events(&[completion("a", 3), completion("a", 4)], now);
        let Lookup::Found(record) = index.first_after("a", watermark) else {
            panic!("completion not found");
        };
        assert_eq!(
            record.get("result").and_then(Value::as_str),
            Some("answer 3")
        );
    }

    #[test]
    fn retention_exposes_an_explicit_generation_floor() {
        let now = event::iso_to_epoch("2026-08-09T12:01:00Z").unwrap();
        let mut index = Index::with_limits(Limits {
            max_age_secs: u64::MAX,
            max_records: 2,
            max_bytes: usize::MAX,
        });
        index.push_events(
            &[completion("a", 1), completion("a", 2), completion("a", 3)],
            now,
        );
        assert_eq!(index.floor, 2);
        assert_eq!(index.first_after("a", 0), Lookup::Expired { floor: 2 });
    }

    /// bn-3s2. Retention is by generation, so with one record kept the
    /// retained `b` is older than the discarded `a`; the old check compared
    /// `since` with `b`'s timestamp and answered Pending, and `wait` then
    /// blocked and returned a's *next* answer as this one.
    #[test]
    fn newest_since_expires_when_a_newer_answer_was_pruned_behind_an_older_one() {
        let now = event::iso_to_epoch("2026-08-09T12:01:00Z").unwrap();
        let mut index = Index::with_limits(Limits {
            max_age_secs: u64::MAX,
            max_records: 1,
            max_bytes: usize::MAX,
        });
        index.push_events(&[completion("a", 50), completion("b", 10)], now);
        let since = event::iso_to_epoch("2026-08-09T12:00:30Z").unwrap();
        assert_eq!(index.newest_since("a", since), Lookup::Expired { floor: 2 });
        let reloaded = Index::decode(&index.encode());
        assert_eq!(
            reloaded.newest_since("a", since),
            Lookup::Expired { floor: 2 }
        );
        // Newer than anything discarded: still an honest wait.
        let later = event::iso_to_epoch("2026-08-09T12:00:51Z").unwrap();
        assert_eq!(index.newest_since("a", later), Lookup::Pending);
    }

    #[test]
    fn legacy_file_without_pruned_epoch_takes_the_newest_retained_timestamp() {
        let at =
            |second: u64| event::iso_to_epoch(&format!("2026-08-09T12:00:{second:02}Z")).unwrap();
        let legacy = |floor: u64| {
            json!({
                "version": VERSION,
                "generation": 3,
                "floor": floor,
                "records": [
                    {"ts": "2026-08-09T12:00:40Z", "session": "b", "generation": 2},
                    {"ts": "2026-08-09T12:00:20Z", "session": "b", "generation": 3},
                ],
            })
            .to_string()
        };
        assert_eq!(Index::decode(&legacy(2)).pruned_epoch, Some(at(40)));
        assert_eq!(
            Index::decode(&legacy(2)).newest_since("a", at(30)),
            Lookup::Expired { floor: 2 }
        );
        assert_eq!(
            Index::decode(&legacy(2)).newest_since("a", at(41)),
            Lookup::Pending
        );
        assert_eq!(Index::decode(&legacy(1)).pruned_epoch, None);
    }

    #[test]
    fn a_pruned_record_without_any_timestamp_does_not_move_the_watermark() {
        let text = json!({
            "version": VERSION,
            "generation": 1,
            "floor": 1,
            "pruned_epoch": null,
            "records": [{"session": "a", "generation": 1}],
        })
        .to_string();
        let mut index = Index::decode(&text);
        index.limits.max_records = 0;
        index.prune(0);
        assert_eq!((index.floor, index.pruned_epoch), (2, None));
        assert_eq!(index.newest_since("a", 0), Lookup::Pending);
    }

    #[test]
    fn atomic_file_round_trip_preserves_generation() {
        let now = event::iso_to_epoch("2026-08-09T12:01:00Z").unwrap();
        let mut index = Index::default();
        index.push_events(&[completion("a", 1)], now);
        let dir = std::env::temp_dir().join(format!("agentbus-completion-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("completions.json");
        assert!(index.write_atomic(&path));
        let loaded = Index::load(&path);
        assert_eq!(loaded.generation, 1);
        assert!(matches!(loaded.first_after("a", 0), Lookup::Found(_)));
    }

    fn at(second: u64) -> u64 {
        event::iso_to_epoch(&stamp(second)).unwrap()
    }

    fn unbounded() -> Limits {
        Limits {
            max_age_secs: u64::MAX,
            max_records: usize::MAX,
            max_bytes: usize::MAX,
        }
    }

    /// bn-351. A record exactly `max_age_secs` old is retained; one second
    /// older is pruned.
    #[test]
    fn age_cutoff_keeps_a_record_exactly_max_age_old() {
        let limits = Limits {
            max_age_secs: 20,
            ..unbounded()
        };
        let mut index = Index::with_limits(limits);
        index.push_events(&[completion("a", 10)], at(30));
        assert_eq!(index.records.len(), 1);
        assert_eq!(index.floor, 1);

        let mut index = Index::with_limits(limits);
        index.push_events(&[completion("a", 10)], at(31));
        assert!(index.records.is_empty());
        assert_eq!((index.floor, index.pruned_epoch), (2, Some(at(10))));
    }

    /// Records are ordered by generation, not time, so age pruning stops at
    /// the first record that is young enough, even with an older one behind.
    #[test]
    fn age_pruning_stops_at_a_young_front_record() {
        let mut index = Index::with_limits(Limits {
            max_age_secs: 20,
            ..unbounded()
        });
        index.push_events(&[completion("a", 50), completion("b", 10)], at(60));
        assert_eq!(index.records.len(), 2);
        assert_eq!((index.floor, index.pruned_epoch), (1, None));
        // Once the front expires the old record behind it goes too.
        index.push_events(&[completion("c", 55)], at(71));
        assert_eq!(index.records.len(), 1);
        assert_eq!(index.floor, 3);
    }

    #[test]
    fn record_cap_keeps_exactly_max_records() {
        let mut index = Index::with_limits(Limits {
            max_records: 3,
            ..unbounded()
        });
        index.push_events(&[completion("a", 1), completion("a", 2)], at(30));
        assert_eq!(index.records.len(), 2);
        index.push_events(&[completion("a", 3)], at(30));
        assert_eq!((index.records.len(), index.floor), (3, 1));
        index.push_events(&[completion("a", 4)], at(30));
        assert_eq!((index.records.len(), index.floor), (3, 2));
    }

    /// Three records of equal shape, with the encoded length of the index at
    /// each of 3, 2 and 1 retained records.
    fn sized_index() -> (Index, [usize; 3]) {
        let mut index = Index::with_limits(unbounded());
        index.push_events(
            &[completion("a", 1), completion("a", 2), completion("a", 3)],
            at(30),
        );
        let mut probe = index.clone();
        let full = probe.encode().len();
        probe.remove_oldest();
        let two = probe.encode().len();
        probe.remove_oldest();
        let one = probe.encode().len();
        assert!(one < two && two < full);
        (index, [full, two, one])
    }

    fn pruned_to(max_bytes: usize) -> Index {
        let (mut index, _) = sized_index();
        index.limits.max_bytes = max_bytes;
        index.prune(at(30));
        index
    }

    #[test]
    fn byte_cap_keeps_an_index_exactly_at_the_limit() {
        let (_, [full, two, one]) = sized_index();
        for (max_bytes, kept) in [(full, 3), (full - 1, 2), (two, 2), (two - 1, 1), (one, 1)] {
            let index = pruned_to(max_bytes);
            assert_eq!(index.records.len(), kept, "max_bytes {max_bytes}");
            if kept > 1 {
                assert!(index.encode().len() <= max_bytes);
            }
        }
    }

    #[test]
    fn byte_cap_prunes_oldest_first_and_records_the_floor() {
        let (_, [_, two, _]) = sized_index();
        let index = pruned_to(two - 1);
        assert_eq!(index.floor, 3);
        assert_eq!(index.pruned_epoch, Some(at(2)));
        assert_eq!(index.records[0]["generation"], json!(3));
    }

    #[test]
    fn byte_cap_never_prunes_the_last_record() {
        let (_, [_, _, one]) = sized_index();
        for max_bytes in [one - 1, 1, 0] {
            let index = pruned_to(max_bytes);
            assert_eq!(index.records.len(), 1, "max_bytes {max_bytes}");
            assert!(index.encode().len() > max_bytes);
            assert_eq!(index.records[0]["generation"], json!(3));
        }
        // A lone record over the cap survives a push too.
        let mut index = Index::with_limits(Limits {
            max_bytes: 0,
            ..unbounded()
        });
        index.push_events(&[completion("a", 1)], at(30));
        assert_eq!(index.records.len(), 1);
    }

    /// bn-wo6: `Index` checked against an unbounded reference log.
    ///
    /// The contract `wait` rests on. With `r` the first reference record for
    /// `s` with generation above `w`, `first_after(s, w)` is `Expired` exactly
    /// when some record above `w` has been discarded (`w + 1 < floor`), and
    /// otherwise `Found(r)` when `r` exists and `Pending` when it does not.
    /// So it never answers `Pending` over a discarded `r`, and never with a
    /// record other than `r`.
    ///
    /// With `r` the newest (highest-generation, the order turns were
    /// published in) reference record for `s` with timestamp at or after
    /// `since`, `newest_since(s, since)` is:
    ///   - `Found(r)` if `r` is retained, and nothing else will do;
    ///   - `Expired` if `r` was discarded;
    ///   - with no `r`, `Pending`, or `Expired` provided some discarded record
    ///     of *any* session has a timestamp at or after `since`. The index
    ///     keeps one watermark rather than one per session, so it may refuse
    ///     a wait it could have kept; it must never keep one it should refuse.
    ///
    /// Every `Expired` carries the index's current floor.
    ///
    /// After every step the structure holds: retained generations strictly
    /// increase within `[floor, generation]` and match the reference record
    /// of the same generation; everything below `floor` was discarded, so
    /// retention is a suffix; `pruned_epoch` is the newest discarded
    /// timestamp; and generation, floor and pruned_epoch never decrease.
    ///
    /// Sampled rather than proved, deliberately. Each operation preserves
    /// this on its own — push, prune, encode/load and the two reads each take
    /// a state satisfying it to one that does, and the monotone parts compose
    /// by transitivity — so there is no inductive content a deductive
    /// verifier would add beyond bookkeeping, and short random sequences with
    /// small limits exercise every step from every shape of state that
    /// matters.
    mod model {
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestCaseError, TestRunner};
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

        const SESSIONS: [&str; 3] = ["a", "b", "c"];

        fn base() -> u64 {
            event::iso_to_epoch("2026-08-09T12:00:00Z").unwrap()
        }

        #[derive(Clone, Debug)]
        enum Stamp {
            At(u64),
            Empty,
            Garbage,
        }

        #[derive(Clone, Debug)]
        struct Ev {
            session: usize,
            stamp: Stamp,
            turn_end: bool,
            long: bool,
        }

        /// A watermark either absolute or counted back from the current
        /// generation, since with a retained window of a few records an
        /// absolute one rarely lands inside it.
        #[derive(Clone, Debug)]
        enum Mark {
            At(u64),
            Back(u64),
        }

        #[derive(Clone, Debug)]
        enum Op {
            Push { events: Vec<Ev>, now: u64 },
            RoundTrip,
            FirstAfter { session: usize, watermark: Mark },
            NewestSince { session: usize, since: u64 },
        }

        struct Reference {
            generation: u64,
            session: String,
            epoch: u64,
            marker: String,
        }

        /// How often each outcome the property is about was reached, so a
        /// generator that stopped producing it fails loudly instead of
        /// passing vacuously.
        #[derive(Default)]
        struct Seen {
            pruned: AtomicUsize,
            first_found: AtomicUsize,
            first_pending: AtomicUsize,
            first_expired: AtomicUsize,
            since_found: AtomicUsize,
            since_pending: AtomicUsize,
            since_expired_required: AtomicUsize,
            since_expired_allowed: AtomicUsize,
            since_pruned_behind_older: AtomicUsize,
        }

        fn stamp() -> impl Strategy<Value = Stamp> {
            prop_oneof![
                8 => (0u64..90).prop_map(Stamp::At),
                1 => Just(Stamp::Empty),
                1 => Just(Stamp::Garbage),
            ]
        }

        fn ev() -> impl Strategy<Value = Ev> {
            (
                0..SESSIONS.len(),
                stamp(),
                prop::bool::weighted(0.8),
                prop::bool::weighted(0.1),
            )
                .prop_map(|(session, stamp, turn_end, long)| Ev {
                    session,
                    stamp,
                    turn_end,
                    long,
                })
        }

        /// Watermarks and `since` values reach below the floor, exactly onto
        /// retained generations and timestamps, and past the end.
        fn op() -> impl Strategy<Value = Op> {
            prop_oneof![
                4 => (prop::collection::vec(ev(), 0..5), 0u64..200)
                    .prop_map(|(events, now)| Op::Push { events, now: base() + now }),
                1 => Just(Op::RoundTrip),
                3 => (
                    0..SESSIONS.len(),
                    prop_oneof![
                        4 => (0u64..40).prop_map(Mark::At),
                        1 => Just(Mark::At(u64::MAX)),
                        5 => (0u64..8).prop_map(Mark::Back),
                    ],
                )
                    .prop_map(|(session, watermark)| Op::FirstAfter { session, watermark }),
                3 => (0..SESSIONS.len(), prop_oneof![9 => (0u64..100).prop_map(|s| base() + s), 1 => Just(0u64)])
                    .prop_map(|(session, since)| Op::NewestSince { session, since }),
            ]
        }

        fn limits() -> impl Strategy<Value = Limits> {
            (5u64..120, 1usize..5, 250usize..1500).prop_map(
                |(max_age_secs, max_records, max_bytes)| Limits {
                    max_age_secs,
                    max_records,
                    max_bytes,
                },
            )
        }

        fn to_event(e: &Ev, marker: &str) -> Event {
            let ts = match e.stamp {
                Stamp::At(offset) => {
                    format!("2026-08-09T12:{:02}:{:02}Z", offset / 60, offset % 60)
                }
                Stamp::Empty => String::new(),
                Stamp::Garbage => "yesterday-ish".into(),
            };
            let text = if e.long {
                format!("{marker} {}", "x".repeat(400))
            } else {
                marker.to_string()
            };
            Event {
                ts,
                source: "test",
                session: SESSIONS[e.session].into(),
                kind: if e.turn_end {
                    Kind::TurnEnd {
                        duration_ms: Some(1),
                        result: Some(marker.to_string()),
                        result_full: Some(text),
                    }
                } else {
                    Kind::Prompt { text }
                },
            }
        }

        fn generation_of(record: &Value) -> u64 {
            record.get("generation").and_then(Value::as_u64).unwrap()
        }

        fn is(found: &Value, reference: &Reference) -> bool {
            generation_of(found) == reference.generation
                && found.get("session").and_then(Value::as_str) == Some(reference.session.as_str())
                && found.get("result").and_then(Value::as_str) == Some(reference.marker.as_str())
        }

        fn run(limits: Limits, ops: &[Op], seen: &Seen) -> Result<(), TestCaseError> {
            let mut index = Index::with_limits(limits);
            let mut log: Vec<Reference> = Vec::new();
            let mut previous = (index.generation, index.floor, index.pruned_epoch);
            let mut markers = 0usize;

            for op in ops {
                match op {
                    Op::Push { events, now } => {
                        let before = index.records.len();
                        let batch: Vec<Event> = events
                            .iter()
                            .map(|e| {
                                markers += 1;
                                to_event(e, &format!("m{markers}"))
                            })
                            .collect();
                        for event in &batch {
                            if let Kind::TurnEnd { result, .. } = &event.kind {
                                log.push(Reference {
                                    generation: log.len() as u64 + 1,
                                    session: event.session.clone(),
                                    epoch: event::iso_to_epoch(&event.ts).unwrap_or(*now),
                                    marker: result.clone().unwrap(),
                                });
                            }
                        }
                        let changed = index.push_events(&batch, *now);
                        prop_assert_eq!(
                            changed,
                            batch.iter().any(|e| matches!(e.kind, Kind::TurnEnd { .. }))
                        );
                        let added = log.len() as u64 - previous.0;
                        if index.records.len() < before + added as usize {
                            seen.pruned.fetch_add(1, Relaxed);
                        }
                        prop_assert!(index.records.len() <= limits.max_records);
                        prop_assert!(
                            index.records.len() == 1 || index.encode().len() <= limits.max_bytes,
                            "{} records encode to {} bytes over {}",
                            index.records.len(),
                            index.encode().len(),
                            limits.max_bytes
                        );
                    }
                    Op::RoundTrip => {
                        let mut loaded = Index::decode(&index.encode());
                        // `load` resets limits to the defaults, which would
                        // stop pruning; the publisher has its own, so reapply.
                        loaded.limits = limits;
                        prop_assert_eq!(loaded.generation, index.generation);
                        prop_assert_eq!(loaded.floor, index.floor);
                        prop_assert_eq!(loaded.pruned_epoch, index.pruned_epoch);
                        prop_assert_eq!(&loaded.records, &index.records);
                        index = loaded;
                    }
                    Op::FirstAfter { session, watermark } => {
                        let session = SESSIONS[*session];
                        let watermark = &match watermark {
                            Mark::At(w) => *w,
                            Mark::Back(k) => index.generation.saturating_sub(*k),
                        };
                        let floor = model_floor(&index, &log);
                        let got = index.first_after(session, *watermark);
                        let first = log
                            .iter()
                            .find(|r| r.session == session && r.generation > *watermark);
                        if watermark.saturating_add(1) < floor {
                            prop_assert_eq!(got, Lookup::Expired { floor: index.floor });
                            seen.first_expired.fetch_add(1, Relaxed);
                        } else if let Some(first) = first {
                            match got {
                                Lookup::Found(found) => prop_assert!(
                                    is(&found, first),
                                    "first_after({}, {}) = gen {}, reference gen {}",
                                    session,
                                    watermark,
                                    generation_of(&found),
                                    first.generation
                                ),
                                other => prop_assert!(
                                    false,
                                    "first_after({}, {}) = {:?}, reference gen {}",
                                    session,
                                    watermark,
                                    other,
                                    first.generation
                                ),
                            }
                            seen.first_found.fetch_add(1, Relaxed);
                        } else {
                            prop_assert_eq!(got, Lookup::Pending);
                            seen.first_pending.fetch_add(1, Relaxed);
                        }
                    }
                    Op::NewestSince { session, since } => {
                        let session = SESSIONS[*session];
                        let floor = model_floor(&index, &log);
                        let got = index.newest_since(session, *since);
                        let newest = log
                            .iter()
                            .rev()
                            .find(|r| r.session == session && r.epoch >= *since);
                        let pruned_since = log
                            .iter()
                            .any(|r| r.generation < floor && r.epoch >= *since);
                        match (newest, got) {
                            (Some(newest), Lookup::Found(found)) => {
                                prop_assert!(
                                    newest.generation >= floor,
                                    "found a discarded record"
                                );
                                prop_assert!(
                                    is(&found, newest),
                                    "newest_since({}, {}) = gen {}, reference gen {}",
                                    session,
                                    since,
                                    generation_of(&found),
                                    newest.generation
                                );
                                seen.since_found.fetch_add(1, Relaxed);
                            }
                            (Some(newest), Lookup::Expired { floor: reported }) => {
                                prop_assert!(
                                    newest.generation < floor,
                                    "Expired over retained gen {}",
                                    newest.generation
                                );
                                prop_assert_eq!(reported, index.floor);
                                seen.since_expired_required.fetch_add(1, Relaxed);
                                let older_first = index
                                    .records
                                    .first()
                                    .and_then(record_epoch)
                                    .is_some_and(|first| *since >= first);
                                if older_first {
                                    seen.since_pruned_behind_older.fetch_add(1, Relaxed);
                                }
                            }
                            (None, Lookup::Pending) => {
                                seen.since_pending.fetch_add(1, Relaxed);
                            }
                            (None, Lookup::Expired { floor: reported }) => {
                                prop_assert!(pruned_since, "newest_since({}, {}) Expired with nothing discarded at or after it", session, since);
                                prop_assert_eq!(reported, index.floor);
                                seen.since_expired_allowed.fetch_add(1, Relaxed);
                            }
                            (newest, got) => prop_assert!(
                                false,
                                "newest_since({}, {}) = {:?}, reference gen {:?}, floor {}",
                                session,
                                since,
                                got,
                                newest.map(|r| r.generation),
                                floor
                            ),
                        }
                    }
                }

                // Structure, after every step.
                prop_assert_eq!(index.generation, log.len() as u64);
                let mut last = index.floor.saturating_sub(1);
                for record in &index.records {
                    let generation = generation_of(record);
                    prop_assert!(
                        generation > last,
                        "retained generations out of order or below floor"
                    );
                    prop_assert!(generation <= index.generation);
                    prop_assert!(
                        is(record, &log[generation as usize - 1]),
                        "retained gen {} differs from the reference",
                        generation
                    );
                    last = generation;
                }
                let floor = model_floor(&index, &log);
                prop_assert_eq!(
                    index.floor,
                    floor,
                    "floor is not one past the newest discarded generation"
                );
                let pruned_epoch = log
                    .iter()
                    .filter(|r| r.generation < floor)
                    .map(|r| r.epoch)
                    .max();
                prop_assert_eq!(index.pruned_epoch, pruned_epoch);
                let now = (index.generation, index.floor, index.pruned_epoch);
                prop_assert!(
                    now.0 >= previous.0 && now.1 >= previous.1 && now.2 >= previous.2,
                    "regressed: {:?} -> {:?}",
                    previous,
                    now
                );
                previous = now;
            }
            Ok(())
        }

        /// One past the newest reference generation the index no longer
        /// holds, computed from the reference rather than read off `floor`.
        fn model_floor(index: &Index, log: &[Reference]) -> u64 {
            let retained: std::collections::BTreeSet<u64> =
                index.records.iter().map(generation_of).collect();
            log.iter()
                .map(|r| r.generation)
                .filter(|g| !retained.contains(g))
                .max()
                .map_or(1, |g| g + 1)
        }

        #[test]
        fn index_agrees_with_an_unbounded_reference_log() {
            let seen = Seen::default();
            let mut runner = TestRunner::new(Config {
                cases: 512,
                ..Config::default()
            });
            let strategy = (limits(), prop::collection::vec(op(), 1..60));
            runner
                .run(&strategy, |(limits, ops)| run(limits, &ops, &seen))
                .unwrap();

            let counts = [
                ("pruned", &seen.pruned),
                ("first_after Found", &seen.first_found),
                ("first_after Pending", &seen.first_pending),
                ("first_after Expired", &seen.first_expired),
                ("newest_since Found", &seen.since_found),
                ("newest_since Pending", &seen.since_pending),
                (
                    "newest_since Expired over a discarded answer",
                    &seen.since_expired_required,
                ),
                (
                    "newest_since Expired, answer discarded behind an older retained record",
                    &seen.since_pruned_behind_older,
                ),
                (
                    "newest_since Expired with no answer",
                    &seen.since_expired_allowed,
                ),
            ];
            for (what, count) in counts {
                assert!(count.load(Relaxed) > 0, "generator never reached: {what}");
            }
        }
    }

    fn pushed_result(full: &str) -> Value {
        let now = event::iso_to_epoch("2026-08-09T12:01:00Z").unwrap();
        let mut event = completion("a", 1);
        event.kind = Kind::TurnEnd {
            duration_ms: Some(1),
            result: Some("preview".into()),
            result_full: Some(full.into()),
        };
        let mut index = Index::default();
        index.push_events(&[event], now);
        index.records.last().cloned().expect("record retained")
    }

    const TRUNCATION_MARKER: &str = "\n[agentbus: completion truncated]";

    #[test]
    fn result_limit_is_one_mebibyte() {
        assert_eq!(MAX_RESULT_BYTES, 1_048_576);
    }

    #[test]
    fn result_full_over_the_limit_is_cut_and_marked() {
        let full = "x".repeat(MAX_RESULT_BYTES + 1);
        let record = pushed_result(&full);
        let kept = record["result_full"].as_str().unwrap();
        assert_eq!(
            kept,
            format!("{}{TRUNCATION_MARKER}", &full[..MAX_RESULT_BYTES])
        );
        assert_eq!(record["result_truncated"], json!(true));
        // The preview is a separate field the clamp never reads or changes.
        assert_eq!(record["result"], json!("preview"));
    }

    #[test]
    fn result_full_cut_ends_on_the_last_char_boundary_before_the_limit() {
        // A 3-byte character starting one byte before the limit straddles it.
        let mut full = "x".repeat(MAX_RESULT_BYTES - 1);
        full.push('€');
        full.push_str("tail");
        assert!(!full.is_char_boundary(MAX_RESULT_BYTES));
        let record = pushed_result(&full);
        let kept = record["result_full"].as_str().unwrap();
        let prefix = kept.strip_suffix(TRUNCATION_MARKER).expect("marker");
        assert_eq!(prefix.len(), MAX_RESULT_BYTES - 1);
        assert_eq!(prefix, &full[..MAX_RESULT_BYTES - 1]);
        assert_eq!(record["result_truncated"], json!(true));
    }

    #[test]
    fn result_full_at_the_limit_is_untouched() {
        let full = "x".repeat(MAX_RESULT_BYTES);
        let record = pushed_result(&full);
        assert_eq!(record["result_full"], json!(full));
        assert!(record.get("result_truncated").is_none());
    }
}
