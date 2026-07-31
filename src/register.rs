//! Session -> pane registrations, and whether they are still true.
//!
//! Transcripts say everything about a session except where it is. This is the
//! one fact that has to be reported rather than observed, so it is kept
//! deliberately small: an identity bridge, not a second data source.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct Pane {
    /// Which multiplexer, so a subscriber can ignore locations it cannot render.
    pub mux: String,
    pub mux_session: String,
    pub pane: String,
    /// The session's own transcript. Claude keeps each subagent's sidecar in a
    /// directory derived from this path, which is the only place a hook-reported
    /// subagent's name can be found.
    pub transcript: String,
    pub pid: u64,
    /// Process start time, which makes the pid unambiguous across reuse.
    pub starttime: u64,
}

/// Read every registration, newest wins. Re-read in full rather than tailed:
/// these are idempotent facts, so replaying them is free and the file can be
/// truncated by the hook without losing anything that still matters.
pub fn load(path: &Path) -> BTreeMap<String, Pane> {
    let mut out = BTreeMap::new();
    let Ok(txt) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in txt.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let session = s("session_id");
        if session.is_empty() {
            continue;
        }
        out.insert(
            session,
            Pane {
                mux: s("mux"),
                mux_session: s("mux_session"),
                pane: s("pane"),
                transcript: s("transcript"),
                pid: v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0),
                starttime: v.get("starttime").and_then(|x| x.as_u64()).unwrap_or(0),
            },
        );
    }
    out
}

/// A registration holds while the exact process that made it is still alive.
///
/// Identity is pid + start time, not pid alone: a recycled pid cannot have the
/// same start time, so this cannot end up pointing at an unrelated process. And
/// since a process does not move between panes, "same process still running" is
/// sufficient to keep believing where it lives.
///
/// This deliberately does not read /proc/<pid>/environ, which would confirm the
/// pane directly. That requires ptrace access, and under ptrace_scope=1 only a
/// descendant of the agent has it — so it works when run by hand from inside the
/// pane and fails as a background service, which is how this is actually run.
pub fn still_true(p: &Pane) -> bool {
    if p.pid == 0 {
        // No pid resolved, so liveness cannot be checked. Keep it: a mapping
        // that might be stale beats no mapping, and the subscriber drops rows
        // for panes it cannot see anyway.
        return true;
    }
    let Ok(txt) = std::fs::read_to_string(format!("/proc/{}/stat", p.pid)) else {
        return false;
    };
    if p.starttime == 0 {
        // Registered before start time was recorded; liveness alone is all we
        // can check for it.
        return true;
    }
    txt.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|t| t == p.starttime)
        .unwrap_or(false)
}
