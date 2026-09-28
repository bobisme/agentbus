//! Offset-based tailing of many files at once.
//!
//! zcode's `tail::follow` blocks forever on a single path, which is right for a
//! viewer and wrong here: the observer follows every live transcript at once and
//! the set changes while it runs. This keeps per-file cursors and returns
//! whatever is new, so one thread can poll them all.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Size of one read from a followed file.
const READ_BUFFER: usize = 1 << 16;

#[derive(Default)]
struct Cursor {
    offset: u64,
    /// Bytes read that don't yet end in a newline. Agents append whole JSON
    /// objects, but a poll can still land mid-write, and half a line parses as
    /// nothing at best and as the wrong thing at worst. Kept as bytes: a
    /// multibyte character can be cut by a poll or by the read buffer, and
    /// decoding is only sound on a whole line.
    partial: Vec<u8>,
}

/// Durable portion of one tail cursor. The file identity is stored by the
/// checkpoint layer; this type only describes where parsing resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailCheckpoint {
    pub offset: u64,
    pub partial: Vec<u8>,
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
                partial: Vec::new(),
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
                partial: Vec::new(),
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
        self.poll_with_buffer(path, READ_BUFFER)
    }

    fn poll_with_buffer(&mut self, path: &Path, buffer: usize) -> Vec<String> {
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
        let mut buf = vec![0u8; buffer.max(1)];
        while cur.offset < len {
            let n = match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            cur.offset += n as u64;
            cur.partial.extend_from_slice(&buf[..n]);
            // Split on the raw newline byte and decode each line once, whole.
            // 0x0A never occurs inside a multibyte UTF-8 sequence, so a split
            // here cannot cut a character.
            while let Some(idx) = cur.partial.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = cur.partial.drain(..=idx).collect();
                let line = String::from_utf8_lossy(&raw);
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

    #[test]
    fn a_multibyte_character_split_across_polls_is_decoded_once() {
        // bn-1q3: "é" is 0xC3 0xA9. Decoding each chunk on its own turned the
        // two halves into two U+FFFD.
        let path = temp_file("utf8-split");
        std::fs::write(&path, b"x\xC3").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert!(tails.poll(&path).is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"\xA9\n").unwrap();
        assert_eq!(tails.poll(&path), vec!["x\u{e9}"]);
    }

    #[test]
    fn a_multibyte_character_split_across_the_read_buffer_is_decoded_once() {
        let path = temp_file("utf8-buffer");
        // 'a' then "é" repeated: é starts at every odd offset, so the read
        // boundary at byte 65536 falls between the two bytes of one.
        let mut content = format!("a{}", "\u{e9}".repeat(36_000)).into_bytes();
        assert_eq!(content[65_535], 0xC3);
        content.push(b'\n');
        std::fs::write(&path, &content).unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(
            tails.poll(&path),
            vec![format!("a{}", "\u{e9}".repeat(36_000))]
        );
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn truncation_skips_to_the_new_end_and_later_appends_are_not_a_replay() {
        let path = temp_file("truncate");
        std::fs::write(&path, "one\ntwo\nthr").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["one", "two"]);
        // A partial "thr" is held.
        assert_eq!(tails.checkpoints()[0].1.partial, b"thr");

        std::fs::write(&path, "x\n").unwrap();
        assert!(tails.poll(&path).is_empty());
        let cp = tails.checkpoints().pop().unwrap().1;
        assert_eq!(cp.offset, 2);
        assert!(cp.partial.is_empty());

        // The truncated content "x" is not replayed; the append is emitted
        // alone, without the stale "thr" glued on.
        append(&path, b"new\n");
        assert_eq!(tails.poll(&path), vec!["new"]);
    }

    #[test]
    fn truncation_to_empty_resets_the_cursor_to_zero() {
        let path = temp_file("truncate-empty");
        std::fs::write(&path, "abc\n").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["abc"]);
        std::fs::write(&path, "").unwrap();
        assert!(tails.poll(&path).is_empty());
        assert_eq!(tails.checkpoints()[0].1.offset, 0);
        append(&path, b"d\n");
        assert_eq!(tails.poll(&path), vec!["d"]);
    }

    #[test]
    fn an_unchanged_file_yields_nothing_and_keeps_its_cursor() {
        let path = temp_file("unchanged");
        std::fs::write(&path, "abc\n").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["abc"]);
        assert!(tails.poll(&path).is_empty());
        assert_eq!(tails.checkpoints()[0].1.offset, 4);
    }

    #[test]
    fn polling_an_untracked_or_missing_file_yields_nothing() {
        let path = temp_file("untracked");
        std::fs::write(&path, "abc\n").unwrap();
        let mut tails = MultiTail::default();
        assert!(tails.poll(&path).is_empty());
        tails.track(&path, true);
        std::fs::remove_file(&path).unwrap();
        assert!(tails.poll(&path).is_empty());
    }

    #[test]
    fn reset_to_end_skips_existing_content_and_drops_the_partial() {
        let path = temp_file("reset");
        std::fs::write(&path, "one\ntw").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        assert_eq!(tails.poll(&path), vec!["one"]);
        assert_eq!(tails.checkpoints()[0].1.partial, b"tw");

        append(&path, b"o\nthree\n");
        tails.reset_to_end(&path);
        let cp = tails.checkpoints().pop().unwrap().1;
        assert_eq!(cp.offset, std::fs::metadata(&path).unwrap().len());
        assert!(cp.partial.is_empty());
        assert!(tails.poll(&path).is_empty());

        append(&path, b"four\n");
        assert_eq!(tails.poll(&path), vec!["four"]);
    }

    #[test]
    fn reset_to_end_starts_tracking_an_untracked_path() {
        let path = temp_file("reset-untracked");
        std::fs::write(&path, "old\n").unwrap();
        let mut tails = MultiTail::default();
        assert!(!tails.is_tracked(&path));
        tails.reset_to_end(&path);
        assert!(tails.is_tracked(&path));
        assert_eq!(tails.checkpoints()[0].1.offset, 4);
    }

    #[test]
    fn is_tracked_follows_track_and_drop_missing() {
        let path = temp_file("tracked");
        let other = path.with_file_name("other.jsonl");
        std::fs::write(&path, "a\n").unwrap();
        let mut tails = MultiTail::default();
        assert!(!tails.is_tracked(&path));
        tails.track(&path, false);
        assert!(tails.is_tracked(&path));
        assert!(!tails.is_tracked(&other));
        std::fs::remove_file(&path).unwrap();
        tails.drop_missing();
        assert!(!tails.is_tracked(&path));
    }

    #[test]
    fn track_without_from_start_begins_at_the_current_end() {
        let path = temp_file("track-end");
        std::fs::write(&path, "old\n").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, false);
        assert_eq!(tails.checkpoints()[0].1.offset, 4);
        assert!(tails.poll(&path).is_empty());
        append(&path, b"new\n");
        assert_eq!(tails.poll(&path), vec!["new"]);
    }

    #[test]
    fn checkpoints_report_each_path_offset_and_partial_bytes() {
        let a = temp_file("cp-a");
        let b = temp_file("cp-b");
        std::fs::write(&a, b"one\nha\xC3").unwrap();
        std::fs::write(&b, "whole\n").unwrap();
        let mut tails = MultiTail::default();
        assert!(tails.checkpoints().is_empty());
        tails.track(&a, true);
        tails.track(&b, true);
        assert_eq!(tails.poll(&a), vec!["one"]);
        assert_eq!(tails.poll(&b), vec!["whole"]);

        let mut cps = tails.checkpoints();
        cps.sort_by(|x, y| x.0.cmp(&y.0));
        let mut want = vec![
            (
                a.clone(),
                TailCheckpoint {
                    offset: 7,
                    partial: b"ha\xC3".to_vec(),
                },
            ),
            (
                b.clone(),
                TailCheckpoint {
                    offset: 6,
                    partial: Vec::new(),
                },
            ),
        ];
        want.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(cps, want);
    }

    #[test]
    fn restore_never_replaces_an_existing_cursor() {
        let path = temp_file("restore-existing");
        std::fs::write(&path, "abc\n").unwrap();
        let mut tails = MultiTail::default();
        tails.track(&path, true);
        tails.restore(
            &path,
            TailCheckpoint {
                offset: 99,
                partial: b"zzz".to_vec(),
            },
        );
        assert_eq!(tails.poll(&path), vec!["abc"]);
    }

    mod property {
        use super::*;
        use crate::cursor;
        use proptest::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Pieces chosen to hit every case the tailer distinguishes: one to
        /// four byte characters, bytes that are never valid UTF-8, lone and
        /// paired carriage returns, and blank lines.
        fn piece() -> impl Strategy<Value = Vec<u8>> {
            prop_oneof![
                3 => "[a-z {}\":]{0,6}".prop_map(String::into_bytes),
                2 => Just("\u{e9}".as_bytes().to_vec()),
                2 => Just("\u{20ac}".as_bytes().to_vec()),
                2 => Just("\u{1f600}".as_bytes().to_vec()),
                1 => Just(vec![0xff]),
                1 => Just(vec![0xc3]),
                1 => Just(vec![0x80]),
                1 => Just(vec![0xf0, 0x9f]),
                1 => Just(b"\r".to_vec()),
                2 => Just(b"\n".to_vec()),
                1 => Just(b"\r\n".to_vec()),
            ]
        }

        #[derive(Clone, Debug)]
        enum Checkpoint {
            None,
            Direct,
            ThroughDisk,
        }

        /// (append this many bytes next, poll afterwards?, checkpoint kind)
        type Step = (usize, bool, Checkpoint);

        fn step() -> impl Strategy<Value = Step> {
            (
                any::<usize>(),
                any::<bool>(),
                prop_oneof![
                    2 => Just(Checkpoint::None),
                    1 => Just(Checkpoint::Direct),
                    1 => Just(Checkpoint::ThroughDisk),
                ],
            )
        }

        /// Which read size to poll with, and whether to put more than 64 KiB
        /// in front so the real buffer boundary is crossed as well.
        fn scenario() -> impl Strategy<Value = (usize, bool)> {
            prop_oneof![
                4 => (1usize..=9, Just(false)),
                1 => (Just(READ_BUFFER), Just(true)),
                1 => (Just(4096usize), Just(true)),
            ]
        }

        fn oracle(content: &[u8]) -> Vec<String> {
            let end = content
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |i| i + 1);
            content[..end]
                .split_inclusive(|&b| b == b'\n')
                .map(|raw| {
                    String::from_utf8_lossy(raw)
                        .trim_end_matches(['\n', '\r'])
                        .to_string()
                })
                .filter(|line| !line.is_empty())
                .collect()
        }

        fn case_dir() -> PathBuf {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("agentbus-tail-prop-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn through_disk(dir: &Path, path: &Path, tail: TailCheckpoint) -> TailCheckpoint {
            let mut saved = BTreeMap::new();
            saved.insert(
                path.to_path_buf(),
                cursor::SavedCursor::capture(path, tail).unwrap(),
            );
            let file = dir.join("cursors.json");
            std::fs::write(&file, cursor::encode(&saved)).unwrap();
            cursor::load(&file).remove(path).unwrap().tail
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(96))]

            #[test]
            fn every_complete_line_is_emitted_once_whatever_the_chunking(
                pieces in proptest::collection::vec(piece(), 0..40),
                (buffer, big) in scenario(),
                steps in proptest::collection::vec(step(), 1..12),
                whole_prefix_first in any::<bool>(),
            ) {
                let mut content = if big {
                    // é starts at odd offsets: the 64 KiB read boundary
                    // (and 4096) cuts one in half.
                    format!("a{}", "\u{e9}".repeat(36_000)).into_bytes()
                } else {
                    Vec::new()
                };
                content.extend(pieces.concat());

                // Turn the arbitrary numbers into ascending cut points, so the
                // file grows in arbitrary byte-sized chunks.
                // With `whole_prefix_first` nothing is polled until the long
                // prefix is all on disk, so one poll starts at offset 0 and
                // crosses the read boundary at a known place instead of one
                // that earlier random polls have shifted.
                let floor = if big && whole_prefix_first { 72_001 } else { 0 };
                let mut cuts: Vec<usize> = steps
                    .iter()
                    .map(|(at, _, _)| (at % (content.len() + 1)).max(floor))
                    .collect();
                cuts.sort_unstable();

                let dir = case_dir();
                let path = dir.join("t.jsonl");
                std::fs::write(&path, b"").unwrap();
                let mut tails = MultiTail::default();
                tails.track(&path, true);
                let mut emitted = Vec::new();
                let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                let mut written = 0;
                for (cut, (_, poll, checkpoint)) in cuts.iter().zip(&steps) {
                    file.write_all(&content[written..*cut]).unwrap();
                    written = *cut;
                    if *poll {
                        emitted.extend(tails.poll_with_buffer(&path, buffer));
                    }
                    let tail = match checkpoint {
                        Checkpoint::None => continue,
                        _ => tails.checkpoints().pop().map(|(_, cp)| cp),
                    };
                    let Some(tail) = tail else { continue };
                    let tail = match checkpoint {
                        Checkpoint::ThroughDisk => through_disk(&dir, &path, tail),
                        _ => tail,
                    };
                    tails = MultiTail::default();
                    tails.restore(&path, tail);
                }
                file.write_all(&content[written..]).unwrap();
                emitted.extend(tails.poll_with_buffer(&path, buffer));

                let _ = std::fs::remove_dir_all(&dir);
                prop_assert_eq!(emitted, oracle(&content));
            }
        }
    }
}
