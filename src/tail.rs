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

/// Durable portion of one tail cursor. The file identity is stored by the
/// checkpoint layer; this type only describes where parsing resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailCheckpoint {
    pub offset: u64,
    pub partial: String,
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

    /// Restore a cursor whose file identity was already validated by the
    /// caller. Existing cursors are never replaced.
    pub fn restore(&mut self, path: &Path, checkpoint: TailCheckpoint) {
        self.cursors.entry(path.to_path_buf()).or_insert(Cursor {
            offset: checkpoint.offset,
            partial: checkpoint.partial,
        });
    }

    pub fn checkpoints(&self) -> Vec<(PathBuf, TailCheckpoint)> {
        self.cursors
            .iter()
            .map(|(path, cursor)| {
                (
                    path.clone(),
                    TailCheckpoint {
                        offset: cursor.offset,
                        partial: cursor.partial.clone(),
                    },
                )
            })
            .collect()
    }

    /// Rebase a journal cursor after the observer atomically compacts a file it
    /// exclusively owns. Existing retained lines are state, not new events.
    pub fn reset_to_end(&mut self, path: &Path) {
        let offset = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        self.cursors.insert(
            path.to_path_buf(),
            Cursor {
                offset,
                partial: String::new(),
            },
        );
    }

    /// Forget cursors whose source file has actually disappeared.
    ///
    /// A file merely aging outside the discovery window is still a known
    /// transcript: if it becomes active again, its old cursor is the only
    /// thing separating new bytes from a replay of the whole conversation.
    /// Missing files cannot reactivate under the same path without being a new
    /// file, so they are the only safe cursors to discard here.
    pub fn drop_missing(&mut self) {
        self.cursors.retain(|p, _| p.exists());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentbus-tail-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("transcript.jsonl")
    }

    #[test]
    fn an_inactive_cursor_resumes_without_replay() {
        let path = temp_file("resume");
        std::fs::write(&path, "old\n").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["old"]);

        // Discovery omitted the transcript for a while. Keeping the cursor is
        // what makes the later append news rather than a full replay.
        tails.drop_missing();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "new").unwrap();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["new"]);
    }

    #[test]
    fn a_removed_file_loses_its_old_cursor() {
        let path = temp_file("replacement");
        std::fs::write(&path, "old\n").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["old"]);
        std::fs::remove_file(&path).unwrap();
        tails.drop_missing();

        std::fs::write(&path, "replacement\n").unwrap();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["replacement"]);
    }

    #[test]
    fn a_partial_line_survives_checkpoint_restore() {
        let path = temp_file("partial");
        std::fs::write(&path, "hel").unwrap();
        let mut first = MultiTail::default();
        first.track(&path, true);
        assert!(first.poll(&path).is_empty());
        let checkpoint = first.checkpoints().pop().unwrap().1;

        let mut resumed = MultiTail::default();
        resumed.restore(&path, checkpoint);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "lo").unwrap();
        assert_eq!(resumed.poll(&path), vec!["hello"]);
    }
}
