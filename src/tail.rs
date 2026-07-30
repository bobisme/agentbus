//! Offset-based tailing of many files at once.
//!
//! zcode's `tail::follow` blocks forever on a single path, which is right for a
//! viewer and wrong here: the observer follows every live transcript at once and
//! the set changes while it runs. This keeps per-file cursors and returns
//! whatever is new, so one thread can poll them all.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Cursor {
    offset: u64,
    /// Bytes read that don't yet end in a newline. Agents append whole JSON
    /// objects, but a poll can still land mid-write, and half a line parses as
    /// nothing at best and as the wrong thing at worst.
    partial: String,
}

#[derive(Default)]
pub struct MultiTail {
    cursors: BTreeMap<PathBuf, Cursor>,
}

impl MultiTail {
    /// Begin following a file. `from_start` replays existing content; otherwise
    /// only lines appended after this call are returned — which is what you want
    /// for a file that already has thousands of lines of history.
    pub fn track(&mut self, path: &Path, from_start: bool) {
        if self.cursors.contains_key(path) {
            return;
        }
        let offset = if from_start {
            0
        } else {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        };
        self.cursors.insert(
            path.to_path_buf(),
            Cursor {
                offset,
                partial: String::new(),
            },
        );
    }

    pub fn is_tracked(&self, path: &Path) -> bool {
        self.cursors.contains_key(path)
    }

    pub fn drop_untracked(&mut self, keep: &[PathBuf]) {
        self.cursors.retain(|p, _| keep.contains(p));
    }

    /// Read everything appended since the last poll, as complete lines.
    pub fn poll(&mut self, path: &Path) -> Vec<String> {
        let Some(cur) = self.cursors.get_mut(path) else {
            return Vec::new();
        };
        let len = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return Vec::new(),
        };
        // Shrinking means truncated or replaced. Re-reading from 0 would replay
        // the whole history as if it were new; starting over at the new end is
        // the lesser evil for a monitor.
        if len < cur.offset {
            cur.offset = len;
            cur.partial.clear();
            return Vec::new();
        }
        if len == cur.offset {
            return Vec::new();
        }
        let Ok(mut f) = std::fs::File::open(path) else {
            return Vec::new();
        };
        if f.seek(SeekFrom::Start(cur.offset)).is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        while cur.offset < len {
            let n = match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            cur.offset += n as u64;
            cur.partial.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(idx) = cur.partial.find('\n') {
                let line: String = cur.partial.drain(..=idx).collect();
                let line = line.trim_end_matches(['\n', '\r']);
                if !line.is_empty() {
                    out.push(line.to_string());
                }
            }
        }
        out
    }
}
