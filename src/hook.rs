//! `agentbus hook <event>` — the publisher side, invoked by agents directly.
//!
//! This replaces a set of shell hooks. Those needed `jq` and `zellij` on PATH
//! and silently did nothing when either was missing, spawned several processes
//! per invocation, and forked a background poller to chase a file that the
//! observer already reads. A hook fires on every prompt and every subagent, so
//! that cost is paid constantly.
//!
//! Hooks publish onto the bus; they never talk to zellij. Two destinations,
//! because the two kinds of report have different truth semantics:
//!
//!   register.jsonl — idempotent facts (this session lives in this pane),
//!                    re-read wholesale, so truncation is harmless.
//!   inbox.jsonl    — events (a subagent started), tailed like a transcript.
//!
//! Every path exits 0. A monitoring hook must never be able to wedge an agent.

use serde_json::{json, Value};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

/// Appends of this size or less are atomic under O_APPEND on Linux, which is
/// what lets several agents write one file with no locking. Every line is
/// clamped to stay below it.
const ATOMIC_LIMIT: usize = 4096;

pub fn run(args: &[String], register: &Path, inbox: &Path) {
    let event = args.first().map(|s| s.as_str()).unwrap_or("");

    // Only read stdin when it is a pipe. An agent delivering a hook payload
    // always closes its end, so the read terminates — but a caller that simply
    // spawns this command inherits its own stdin, and if that is the terminal
    // the read never ends. Two things then go wrong at once: the caller waits
    // forever on a command that cannot finish, and this process sits consuming
    // the user's keystrokes from under the TUI they are typing into.
    let mut raw = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_string(&mut raw);
    }
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    match event {
        "register" => register_session(&payload, register),
        "subagent" => subagent(&payload, args.get(1).map(|s| s.as_str()), inbox),
        "state" => state(
            &payload,
            args.get(1).map(|s| s.as_str()),
            args.get(2),
            inbox,
        ),
        _ => {}
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

/// Where this process is running, as (multiplexer, session, pane).
///
/// Every multiplexer exports its own pane identity, so the same walk works for
/// all of them and a subscriber can ask for the one it renders. Empty pane means
/// "not in one" — a bare terminal, a CI job, a PTY runtime nobody here has heard
/// of. That is reported as an empty location rather than guessed at: a
/// subscriber already has to handle one, since a location is cleared the moment
/// its binding goes stale.
fn location() -> (&'static str, String, String) {
    let zellij = env("ZELLIJ_PANE_ID");
    if !zellij.is_empty() {
        return ("zellij", env("ZELLIJ_SESSION_NAME"), zellij);
    }
    let tmux = env("TMUX_PANE");
    if !tmux.is_empty() {
        // TMUX is "<socket>,<pid>,<session>"; the last field is the session.
        let sess = env("TMUX")
            .rsplit(',')
            .next()
            .unwrap_or_default()
            .to_string();
        return ("tmux", sess, tmux);
    }
    let wezterm = env("WEZTERM_PANE");
    if !wezterm.is_empty() {
        return ("wezterm", String::new(), wezterm);
    }
    let kitty = env("KITTY_WINDOW_ID");
    if !kitty.is_empty() {
        return ("kitty", String::new(), kitty);
    }
    ("", String::new(), String::new())
}

/// Collapse to one line and clamp, so the record stays atomically appendable
/// and cannot corrupt the lines around it. Control characters are removed
/// rather than escaped: this text ends up rendered in a terminal pane.
fn clean(v: &str, max: usize) -> String {
    let out: String = v
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > max {
        out.chars().take(max).collect()
    } else {
        out
    }
}

fn append(path: &Path, line: &str) {
    if line.len() > ATOMIC_LIMIT {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// /proc/<pid>/stat's comm field can contain spaces and parens, so fields
/// cannot be counted from the left; everything after the last ')' is fixed.
fn ppid_of(pid: u32) -> Option<u32> {
    let txt = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = txt.rsplit_once(')')?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Field 22 of /proc/<pid>/stat: the process start time, in clock ticks since
/// boot. Pairing it with the pid makes the identity exact — a recycled pid
/// cannot have the same start time — and unlike /proc/<pid>/environ it is
/// world-readable, so a daemon can check it. environ requires ptrace access,
/// which under ptrace_scope=1 only a descendant of the agent has.
fn starttime(pid: u32) -> u64 {
    let Ok(txt) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return 0;
    };
    // comm can contain spaces and parens, so count fields only after the last
    // ')': what follows is state, ppid, ... and start time is the 20th of those.
    txt.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Walk up to the agent process itself. Recording its pid, paired with its
/// start time, is what lets the observer decide staleness exactly — the mapping
/// holds while that exact process lives — rather than with a timeout.
fn agent_pid() -> u32 {
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..8 {
        let c = cmdline(pid);
        if c.is_empty() {
            break;
        }
        let names_agent = ["claude", "codex", "opencode"]
            .iter()
            .any(|n| c.contains(n));
        // Skip our own command line, which names an agent only because this
        // binary is invoked from an agent's hook configuration.
        if names_agent && !c.contains("agentbus") {
            return pid;
        }
        match ppid_of(pid) {
            Some(p) if p != 0 && p != pid => pid = p,
            _ => break,
        }
    }
    0
}

fn register_session(p: &Value, register: &Path) {
    // A subagent shares its parent's pane and must not register as a session of
    // its own; it already appears nested under the parent.
    if !s(p, "agent_id").is_empty() {
        return;
    }
    let session = s(p, "session_id");
    if session.is_empty() {
        return;
    }
    // A record is written even with no pane to put in it. Most of what this
    // carries has nothing to do with a multiplexer: pid and start time are an
    // exact process identity — the thing that lets a supervisor say "this
    // session is the agent I spawned" instead of guessing from cwd and timing —
    // and the transcript path is the only route to a session's subagent
    // sidecars. Returning early on an unrecognised host threw all of that away
    // to say nothing more than "I do not know where this is".
    let (mux, mux_session, pane) = location();
    let pid = agent_pid();
    let line = json!({
        "session_id": session,
        "transcript": s(p, "transcript_path"),
        "mux": mux,
        "mux_session": mux_session,
        "pane": pane,
        "pid": pid,
        "starttime": starttime(pid),
    });
    append(register, &line.to_string());
}

fn subagent(p: &Value, phase: Option<&str>, inbox: &Path) {
    let agent_id = s(p, "agent_id");
    let session = s(p, "session_id");
    if agent_id.is_empty() || session.is_empty() {
        return;
    }
    // No description chase here, deliberately. Claude writes the sidecar about a
    // second after this fires; the observer reads that file itself, so waiting
    // would only delay the agent to learn something already being watched.
    let line = json!({
        "kind": "subagent",
        "session": session,
        "agent_id": agent_id,
        "event": if phase == Some("stop") { "stop" } else { "start" },
        "agent_type": s(p, "agent_type"),
        "description": clean(&s(p, "description"), 120),
        "result": clean(&s(p, "last_assistant_message"), 160),
    });
    append(inbox, &line.to_string());
}

/// Report from an agent whose state cannot be read off disk — currently
/// OpenCode, whose plugin API sees transitions that reach no transcript.
fn state(p: &Value, st: Option<&str>, detail: Option<&String>, inbox: &Path) {
    let Some(st) = st else { return };
    let (mux, mux_session, pane) = location();
    let pid = agent_pid();
    // Such an agent has no transcript and therefore no session id of its own.
    // Synthesising one from the pane keeps it a first-class row without
    // pretending it was observed — but that needs a pane to name it after, so a
    // report carrying neither is about nothing and is the one case dropped.
    // A report that names its session stands on its own, pane or no pane:
    // "blocked" reaches no transcript, so this is the only way it is ever heard.
    let session = match s(p, "session_id") {
        x if !x.is_empty() => x,
        _ if pane.is_empty() => return,
        _ => format!("pane:{mux}/{mux_session}/{pane}"),
    };
    let line = json!({
        "kind": "state",
        "session": session,
        "state": st,
        "detail": clean(detail.map(|d| d.as_str()).unwrap_or(""), 60),
        "mux": mux,
        "mux_session": mux_session,
        "pane": pane,
        "pid": pid,
        "starttime": starttime(pid),
    });
    append(inbox, &line.to_string());
}

pub fn default_inbox(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox.jsonl")
}
