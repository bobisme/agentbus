//! Bounded completion history and generation watermarks for `agentbus wait`.
//!
//! This projection is deliberately separate from the verbose event log. It is
//! atomically replaced, small enough to read on every wait poll, and carries a
//! monotone generation so a waiter can take its watermark before resolving a
//! session without racing a fast turn.

use crate::event::{self, Event, Kind};
use serde_json::{json, Value};
use std::path::Path;

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
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
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
        Self {
            generation,
            floor,
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
        let oldest_timestamp = self.records.iter().find_map(record_epoch);
        if self.floor > 1 && oldest_timestamp.is_some_and(|oldest| since < oldest) {
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
    }
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

    fn completion(session: &str, second: u64) -> Event {
        Event {
            ts: format!("2026-08-09T12:00:{second:02}Z"),
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
}
