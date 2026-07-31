//! Find the transcripts worth following.
//!
//! There are ~1500 transcripts on this machine and ~6 alive at any moment, so
//! recency filtering is load-bearing rather than an optimisation: tailing them
//! all would open a file handle per historical session.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Claude,
    Codex,
    Agy,
}

pub struct Found {
    pub path: PathBuf,
    pub source: Source,
    /// Set for Claude subagent transcripts, which live in a `subagents/` dir
    /// beside the parent and are named `agent-<id>.jsonl`.
    pub parent_session: Option<String>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

fn recent(path: &std::path::Path, within: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < within)
        .unwrap_or(false)
}

/// Every transcript touched within `within`, across every agent.
pub fn active(within: Duration) -> Vec<Found> {
    let mut out = Vec::new();
    claude(within, &mut out);
    codex(within, &mut out);
    agy(within, &mut out);
    out
}

/// agy: ~/.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript.jsonl
///
/// `transcript_full.jsonl` sits beside it with the same records and untruncated
/// content. Only one is followed — tailing both would double every event — and
/// it is the shorter one, since what is clipped there is tool output rather
/// than anything published.
fn agy(within: Duration, out: &mut Vec<Found>) {
    let root = home().join(".gemini/antigravity-cli/brain");
    for conv in read_dirs(&root) {
        let p = conv.join(".system_generated/logs/transcript.jsonl");
        if p.is_file() && recent(&p, within) {
            out.push(Found {
                path: p,
                source: Source::Agy,
                parent_session: None,
            });
        }
    }
}

/// Claude Code: ~/.claude/projects/<slug>/<session>.jsonl, with subagents in
/// ~/.claude/projects/<slug>/<session>/subagents/agent-<id>.jsonl
fn claude(within: Duration, out: &mut Vec<Found>) {
    let root = home().join(".claude/projects");
    let Ok(projects) = std::fs::read_dir(&root) else {
        return;
    };
    for proj in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(proj.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false) && recent(&p, within) {
                out.push(Found {
                    path: p,
                    source: Source::Claude,
                    parent_session: None,
                });
                continue;
            }
            // A directory named for a session holds that session's subagents.
            let subs = p.join("subagents");
            if !subs.is_dir() {
                continue;
            }
            let parent = p.file_name().map(|s| s.to_string_lossy().to_string());
            let Ok(agents) = std::fs::read_dir(&subs) else {
                continue;
            };
            for a in agents.flatten() {
                let ap = a.path();
                if ap.extension().map(|x| x == "jsonl").unwrap_or(false) && recent(&ap, within) {
                    out.push(Found {
                        path: ap,
                        source: Source::Claude,
                        parent_session: parent.clone(),
                    });
                }
            }
        }
    }
}

/// Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl. Subagents get
/// their own rollout rather than a nested directory, so parentage is only
/// visible from the parent's own stream.
fn codex(within: Duration, out: &mut Vec<Found>) {
    let root = home().join(".codex/sessions");
    // Walk year/month/day without pulling in a recursive-walk dependency; the
    // layout is fixed and exactly three levels deep.
    for depth1 in read_dirs(&root) {
        for depth2 in read_dirs(&depth1) {
            for day in read_dirs(&depth2) {
                let Ok(files) = std::fs::read_dir(&day) else {
                    continue;
                };
                for f in files.flatten() {
                    let p = f.path();
                    let is_rollout = p
                        .file_name()
                        .map(|n| n.to_string_lossy().starts_with("rollout-"))
                        .unwrap_or(false);
                    if is_rollout
                        && p.extension().map(|x| x == "jsonl").unwrap_or(false)
                        && recent(&p, within)
                    {
                        out.push(Found {
                            path: p,
                            source: Source::Codex,
                            parent_session: None,
                        });
                    }
                }
            }
        }
    }
}

fn read_dirs(path: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}
