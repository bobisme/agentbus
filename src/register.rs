//! Session -> pane registrations, and whether they are still true.
//!
//! Transcripts say everything about a session except where it is. This is the
//! one fact that has to be reported rather than observed, so it is kept
//! deliberately small: an identity bridge, not a second data source.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Rewrite append history as one newest record per session. Hooks publish into
/// a separate spool, so this observer-owned replace cannot race a hook writer.
pub fn compact(path: &Path, max_sessions: usize) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut latest: BTreeMap<String, (usize, Value)> = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(session) = value.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        if session.is_empty() {
            continue;
        }
        latest.insert(session.to_string(), (index, value));
    }
    let mut records: Vec<(usize, Value)> = latest.into_values().collect();
    records.sort_by_key(|(index, _)| *index);
    if records.len() > max_sessions {
        records.drain(..records.len() - max_sessions);
    }
    let mut compacted = records
        .into_iter()
        .map(|(_, value)| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if !compacted.is_empty() {
        compacted.push('\n');
    }
    if compacted == text {
        return false;
    }
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, compacted).is_ok() && std::fs::rename(tmp, path).is_ok()
}

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
    /// A process serving many sessions, set instead of `pid` when the hook ran
    /// under one (codex's shared app-server). Nonzero `pid` and `host_pid`
    /// never coexist.
    pub host_pid: u64,
    pub host_starttime: u64,
    /// When the hook registered this session, in clock ticks since boot, the
    /// unit of a process start time. Only recorded for a hosted session.
    pub registered_tick: u64,
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
        let number = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
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
                pid: number("pid"),
                starttime: number("starttime"),
                host_pid: number("host_pid"),
                host_starttime: number("host_starttime"),
                registered_tick: number("registered_tick"),
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

/// Whether this registration proves that the exact process exists right now.
///
/// Unlike `still_true`, this is intentionally strict: legacy registrations
/// without a pid or start time remain useful as pane hints, but cannot support
/// a system-wide claim that an agent is running. Zombies are no longer agents
/// either, even though their `/proc` entry has not yet been reaped.
pub fn exactly_live(p: &Pane) -> bool {
    process_is(p.pid, p.starttime)
}

/// `exactly_live` for the host a session was registered under. A live host
/// says the session can still be served, not that anyone is attached to it.
pub fn host_live(p: &Pane) -> bool {
    process_is(p.host_pid, p.host_starttime)
}

fn process_is(pid: u64, starttime: u64) -> bool {
    if pid == 0 || starttime == 0 {
        return false;
    }
    let Ok(txt) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    parse_stat(&txt)
        .map(|(state, started)| state != 'Z' && started == starttime)
        .unwrap_or(false)
}

/// `/proc/<pid>/stat` has a parenthesized command that may itself contain
/// spaces and parentheses, so fixed fields can only be counted after the last
/// `)`. Returns process state (field 3) and start time (field 22).
pub(crate) fn parse_stat(txt: &str) -> Option<(char, u64)> {
    let rest = txt.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let starttime = fields.nth(18)?.parse().ok()?;
    Some((state, starttime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(state: char, starttime: u64) -> String {
        let between = std::iter::repeat_n("0", 18).collect::<Vec<_>>().join(" ");
        format!("123 (a command ) with spaces) {state} {between} {starttime} 0")
    }

    #[test]
    fn parses_state_and_starttime_after_a_strange_command() {
        assert_eq!(parse_stat(&stat('S', 987)), Some(('S', 987)));
    }

    #[test]
    fn zero_identity_is_never_exactly_live() {
        assert!(!exactly_live(&Pane::default()));
    }

    #[test]
    fn current_process_is_exactly_live() {
        let pid = std::process::id() as u64;
        let txt = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let (_, starttime) = parse_stat(&txt).unwrap();
        assert!(exactly_live(&Pane {
            pid,
            starttime,
            ..Default::default()
        }));
    }

    #[test]
    fn compaction_keeps_the_newest_bounded_registration_per_session() {
        let dir = std::env::temp_dir().join(format!("agentbus-register-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("register.jsonl");
        std::fs::write(
            &path,
            "{\"session_id\":\"a\",\"pane\":\"old\"}\n{\"session_id\":\"b\",\"pane\":\"b\"}\n{\"session_id\":\"a\",\"pane\":\"new\"}\n",
        )
        .unwrap();
        assert!(compact(&path, 2));
        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["a"].pane, "new");
    }
}
