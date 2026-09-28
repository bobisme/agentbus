//! Size- and age-bounded generations for the verbose normalized event log.

use crate::event::Event;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub generation_bytes: u64,
    pub max_bytes: u64,
    pub max_age_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            generation_bytes: 64 * 1024 * 1024,
            max_bytes: 512 * 1024 * 1024,
            max_age_secs: 7 * 24 * 60 * 60,
        }
    }
}

pub fn append(path: &Path, batch: &[Event], limits: Limits, now: u64) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    for event in batch {
        serde_json::to_writer(&mut bytes, &event.to_json())
            .map_err(|error| format!("cannot encode event: {error}"))?;
        bytes.push(b'\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    }
    let current = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if current > 0 && current.saturating_add(bytes.len() as u64) > limits.generation_bytes {
        rotate(path, current, limits.generation_bytes, now)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("cannot append {}: {error}", path.display()))?;
    prune(path, limits, now)
}

fn rotate(path: &Path, current: u64, target: u64, now: u64) -> Result<(), String> {
    // A file more than twice the configured generation size predates bounded
    // generations. Preserve it outside the automatic deletion namespace for a
    // verified one-time cutover rather than deleting it on first startup.
    let kind = if current > target.saturating_mul(2) {
        "legacy"
    } else {
        "sealed"
    };
    let target = unique_generation(path, now, kind);
    std::fs::rename(path, &target).map_err(|error| {
        format!(
            "cannot rotate {} to {}: {error}",
            path.display(),
            target.display()
        )
    })
}

fn unique_generation(path: &Path, now: u64, kind: &str) -> PathBuf {
    for sequence in 0u32.. {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{now:020}.{sequence:04}.{kind}"));
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 generation namespace exhausted")
}

fn sealed_prefix(path: &Path) -> OsString {
    let mut prefix = path.file_name().unwrap_or_default().to_os_string();
    prefix.push(".");
    prefix
}

fn sealed_generations(path: &Path) -> Vec<(PathBuf, u64, u64)> {
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let prefix = sealed_prefix(path);
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .as_encoded_bytes()
            .starts_with(prefix.as_encoded_bytes())
            || !name.as_encoded_bytes().ends_with(b".sealed")
        {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        found.push((entry.path(), meta.len(), modified));
    }
    found.sort_by_key(|(_, _, modified)| *modified);
    found
}

fn prune(path: &Path, limits: Limits, now: u64) -> Result<(), String> {
    let cutoff = now.saturating_sub(limits.max_age_secs);
    let mut generations = sealed_generations(path);
    for (candidate, _, modified) in &generations {
        if *modified >= cutoff {
            continue;
        }
        std::fs::remove_file(candidate)
            .map_err(|error| format!("cannot prune {}: {error}", candidate.display()))?;
    }
    generations = sealed_generations(path);
    let active = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let mut total = generations
        .iter()
        .fold(active, |sum, (_, bytes, _)| sum.saturating_add(*bytes));
    for (candidate, bytes, _) in generations {
        if total <= limits.max_bytes {
            break;
        }
        std::fs::remove_file(&candidate)
            .map_err(|error| format!("cannot prune {}: {error}", candidate.display()))?;
        total = total.saturating_sub(bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;

    fn event(n: u64) -> Event {
        Event {
            ts: "2026-08-09T12:00:00Z".into(),
            source: "test",
            session: "s".into(),
            kind: Kind::Label {
                text: format!("event-{n}"),
            },
        }
    }

    fn fixture(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentbus-event-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("events.jsonl")
    }

    #[test]
    fn rotation_preserves_every_complete_event_once() {
        let path = fixture("rotation");
        let limits = Limits {
            generation_bytes: 100,
            max_bytes: 10_000,
            max_age_secs: u64::MAX,
        };
        for n in 0..4 {
            append(&path, &[event(n)], limits, 100 + n).unwrap();
        }
        let mut lines = Vec::new();
        for (sealed, _, _) in sealed_generations(&path) {
            lines.extend(
                std::fs::read_to_string(sealed)
                    .unwrap()
                    .lines()
                    .map(str::to_string),
            );
        }
        lines.extend(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(str::to_string),
        );
        assert_eq!(lines.len(), 4);
        let mut unique = lines.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn byte_cap_prunes_oldest_sealed_generations() {
        let path = fixture("cap");
        let limits = Limits {
            generation_bytes: 100,
            max_bytes: 220,
            max_age_secs: u64::MAX,
        };
        for n in 0..8 {
            append(&path, &[event(n)], limits, 100 + n).unwrap();
        }
        let total = sealed_generations(&path)
            .iter()
            .map(|(_, bytes, _)| *bytes)
            .sum::<u64>()
            + std::fs::metadata(&path).unwrap().len();
        assert!(total <= limits.max_bytes);
    }

    #[test]
    fn oversized_legacy_file_is_preserved_outside_pruning_namespace() {
        let path = fixture("legacy");
        std::fs::write(&path, vec![b'x'; 300]).unwrap();
        let limits = Limits {
            generation_bytes: 100,
            max_bytes: 150,
            max_age_secs: 0,
        };
        append(&path, &[event(1)], limits, 10_000).unwrap();
        let legacy = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().as_encoded_bytes().ends_with(b".legacy"));
        assert_eq!(legacy.unwrap().metadata().unwrap().len(), 300);
    }

    #[test]
    fn age_pruning_and_unknown_file_handling_are_conservative() {
        let path = fixture("age");
        let unknown = path.parent().unwrap().join("events.jsonl.operator-backup");
        std::fs::write(&unknown, "keep me").unwrap();
        let limits = Limits {
            generation_bytes: 100,
            max_bytes: 10_000,
            max_age_secs: 1,
        };
        append(&path, &[event(1)], limits, now()).unwrap();
        append(&path, &[event(2)], limits, now()).unwrap();
        assert!(!sealed_generations(&path).is_empty());
        append(&path, &[event(3)], limits, now().saturating_add(10)).unwrap();
        assert!(sealed_generations(&path).is_empty());
        assert_eq!(std::fs::read_to_string(unknown).unwrap(), "keep me");
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}
