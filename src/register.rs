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
    pub zellij_session: String,
    pub pane_id: String,
    pub pid: u64,
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
                zellij_session: s("zellij_session"),
                pane_id: s("pane_id"),
                pid: v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0),
                },
        );
    }
    out
}

/// A registration holds while the agent process is alive *and* its environment
/// still names that pane. Exact rather than heuristic — no TTL, and no chance of
/// pointing at a pane that was closed and its id reused by something else.
pub fn still_true(p: &Pane) -> bool {
    if p.pid == 0 {
        // No pid resolved, so liveness cannot be checked. Keep it: a mapping
        // that might be stale beats no mapping, and the subscriber drops rows
        // for panes it cannot see anyway.
        return true;
    }
    let env_path = format!("/proc/{}/environ", p.pid);
    let Ok(raw) = std::fs::read(&env_path) else {
        return false;
    };
    let mut pane_ok = p.pane_id.is_empty();
    let mut sess_ok = p.zellij_session.is_empty();
    for entry in raw.split(|b| *b == 0) {
        let Ok(kv) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(v) = kv.strip_prefix("ZELLIJ_PANE_ID=") {
            pane_ok = v == p.pane_id;
        } else if let Some(v) = kv.strip_prefix("ZELLIJ_SESSION_NAME=") {
            sess_ok = v == p.zellij_session;
        }
    }
    pane_ok && sess_ok
}
