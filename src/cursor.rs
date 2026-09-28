//! Restart-safe transcript publication cursors.
//!
//! The observer still replays active history to reconstruct the snapshot after
//! a restart, but these cursors say which bytes have already been eligible for
//! publication. A transcript rediscovered later is primed through its saved
//! prefix only to rebuild source-specific parser state; that prefix is never
//! emitted as news.

use crate::tail::TailCheckpoint;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Version 2 stores the partial line as lowercase hex in `partial_hex`, because
/// a partial line is raw bytes and may end inside a multibyte character, which
/// a JSON string cannot hold. Version 1 stored it lossily as a string in
/// `partial`; those files still load, their string becoming its UTF-8 bytes.
const VERSION: u64 = 2;
const OLDEST_READABLE: u64 = 1;

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let raw = text.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    raw.chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}
pub const MAX_ENTRIES: usize = 10_000;

#[derive(Clone, Debug)]
pub struct SavedCursor {
    pub dev: u64,
    pub ino: u64,
    pub modified_ns: u128,
    pub tail: TailCheckpoint,
}

impl SavedCursor {
    pub fn capture(path: &Path, tail: TailCheckpoint) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            modified_ns,
            tail,
        })
    }

    pub fn matches(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|meta| {
                meta.dev() == self.dev && meta.ino() == self.ino && meta.len() >= self.tail.offset
            })
            .unwrap_or(false)
    }
}

pub fn load(path: &Path) -> BTreeMap<PathBuf, SavedCursor> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    let version = match root.get("version").and_then(Value::as_u64) {
        Some(v) if (OLDEST_READABLE..=VERSION).contains(&v) => v,
        _ => return BTreeMap::new(),
    };
    let Some(entries) = root.get("streams").and_then(Value::as_array) else {
        return BTreeMap::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let path = PathBuf::from(entry.get("path")?.as_str()?);
            let dev = entry.get("dev")?.as_u64()?;
            let ino = entry.get("ino")?.as_u64()?;
            let modified_ns = entry
                .get("modified_ns")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let offset = entry.get("offset")?.as_u64()?;
            // An undecodable partial drops the entry: resuming with the wrong
            // bytes would emit a corrupt line, while a dropped cursor is
            // re-seeded at the file's end by the caller.
            let partial = if version == 1 {
                entry
                    .get("partial")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec()
            } else {
                match entry.get("partial_hex").and_then(Value::as_str) {
                    Some(hex) => hex_decode(hex)?,
                    None => Vec::new(),
                }
            };
            Some((
                path,
                SavedCursor {
                    dev,
                    ino,
                    modified_ns,
                    tail: TailCheckpoint { offset, partial },
                },
            ))
        })
        .collect()
}

/// Keep only existing, most-recently-modified transcripts.
pub fn prune(cursors: &mut BTreeMap<PathBuf, SavedCursor>) {
    cursors.retain(|path, saved| saved.matches(path));
    if cursors.len() <= MAX_ENTRIES {
        return;
    }
    let mut oldest: Vec<(PathBuf, u128)> = cursors
        .iter()
        .map(|(path, saved)| (path.clone(), saved.modified_ns))
        .collect();
    oldest.sort_by_key(|(_, modified)| *modified);
    let remove = oldest.len() - MAX_ENTRIES;
    for (path, _) in oldest.into_iter().take(remove) {
        cursors.remove(&path);
    }
}

pub fn encode(cursors: &BTreeMap<PathBuf, SavedCursor>) -> String {
    let streams: Vec<Value> = cursors
        .iter()
        .map(|(path, saved)| {
            json!({
                "path": path.to_string_lossy(),
                "dev": saved.dev,
                "ino": saved.ino,
                "modified_ns": saved.modified_ns.to_string(),
                "offset": saved.tail.offset,
                "partial_hex": hex_encode(&saved.tail.partial),
            })
        })
        .collect();
    serde_json::to_string(&json!({ "version": VERSION, "streams": streams })).unwrap_or_default()
}

pub fn write_atomic(path: &Path, text: &str) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).is_ok() && std::fs::rename(tmp, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_or_newer_checkpoint_is_ignored() {
        let dir = std::env::temp_dir().join(format!("agentbus-cursor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cursors.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_empty());
        std::fs::write(&path, r#"{"version":999,"streams":[]}"#).unwrap();
        assert!(load(&path).is_empty());
    }

    #[test]
    fn checkpoint_round_trips_large_timestamps_without_json_precision_loss() {
        let path = PathBuf::from("/tmp/transcript.jsonl");
        let mut input = BTreeMap::new();
        input.insert(
            path.clone(),
            SavedCursor {
                dev: 2,
                ino: 3,
                modified_ns: u64::MAX as u128 + 99,
                tail: TailCheckpoint {
                    offset: 44,
                    partial: b"half".to_vec(),
                },
            },
        );
        let root: Value = serde_json::from_str(&encode(&input)).unwrap();
        let dir = std::env::temp_dir().join(format!("agentbus-cursor-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("cursors.json");
        std::fs::write(&saved, serde_json::to_string(&root).unwrap()).unwrap();
        let got = load(&saved);
        assert_eq!(got[&path].modified_ns, u64::MAX as u128 + 99);
        assert_eq!(got[&path].tail.partial, b"half");
    }

    #[test]
    fn truncation_invalidates_a_saved_cursor() {
        let dir =
            std::env::temp_dir().join(format!("agentbus-cursor-truncate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, "long history\n").unwrap();
        let saved = SavedCursor::capture(
            &path,
            TailCheckpoint {
                offset: 13,
                partial: Vec::new(),
            },
        )
        .unwrap();
        std::fs::write(&path, "new\n").unwrap();
        assert!(!saved.matches(&path));
    }

    #[test]
    fn a_version_1_file_still_loads() {
        let dir = std::env::temp_dir().join(format!("agentbus-cursor-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("cursors.json");
        std::fs::write(
            &saved,
            r#"{"version":1,"streams":[{"path":"/tmp/t.jsonl","dev":2,"ino":3,
                "modified_ns":"77","offset":44,"partial":"h\u00e9"}]}"#,
        )
        .unwrap();
        let got = load(&saved);
        let cursor = &got[&PathBuf::from("/tmp/t.jsonl")];
        assert_eq!(cursor.tail.offset, 44);
        assert_eq!(cursor.tail.partial, "h\u{e9}".as_bytes());
        assert_eq!(cursor.modified_ns, 77);
    }

    #[test]
    fn a_partial_ending_inside_a_character_round_trips() {
        let path = PathBuf::from("/tmp/transcript.jsonl");
        let mut input = BTreeMap::new();
        let partial = vec![b'x', 0xc3, 0xff, 0x00];
        input.insert(
            path.clone(),
            SavedCursor {
                dev: 1,
                ino: 1,
                modified_ns: 0,
                tail: TailCheckpoint {
                    offset: 4,
                    partial: partial.clone(),
                },
            },
        );
        let dir = std::env::temp_dir().join(format!("agentbus-cursor-hex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("cursors.json");
        std::fs::write(&saved, encode(&input)).unwrap();
        assert_eq!(load(&saved)[&path].tail.partial, partial);
    }
}
