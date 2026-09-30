//! `agentbus hook <event>` — the publisher side, invoked by agents directly.
//!
//! This replaces a set of shell hooks. Those needed `jq` and `zellij` on PATH
//! and silently did nothing when either was missing, spawned several processes
//! per invocation, and forked a background poller to chase a file that the
//! observer already reads. A hook fires on every prompt and every subagent, so
//! that cost is paid constantly.
//!
//! Hooks publish onto the bus; they never talk to zellij. Each invocation writes
//! one unique temporary file and atomically renames it ready. The observer then
//! drains two logical destinations with different truth semantics:
//!
//!   register.jsonl — idempotent facts (this session lives in this pane),
//!                    compacted to the newest fact per session.
//!   inbox.jsonl    — retained events (a subagent started), bounded and tailed
//!                    like a transcript.
//!
//! Every path exits 0. A monitoring hook must never be able to wedge an agent.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bound for one atomic spool record and the legacy append fallback.
const ATOMIC_LIMIT: usize = 4096;
const JOURNAL_MAX_RECORDS: usize = 10_000;
const JOURNAL_MAX_BYTES: usize = 64 * 1024 * 1024;
const JOURNAL_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
static SPOOL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn run(args: &[String], register: &Path, inbox: &Path, spool: &Path) {
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
        "register" => register_session(&payload, register, inbox, spool),
        "turn-end" => turn_end(&payload, inbox, spool),
        "subagent" => subagent(&payload, args.get(1).map(|s| s.as_str()), inbox, spool),
        "state" => state(
            &payload,
            args.get(1).map(|s| s.as_str()),
            args.get(2),
            inbox,
            spool,
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

/// The first of several spellings a payload might use for the same thing.
///
/// Claude and Codex agree on snake_case; agy encodes its payloads with protojson
/// and so spells everything camelCase — `conversationId`, `transcriptPath`. The
/// alternative is a per-agent hook binary, which is what having one normalised
/// bus is meant to avoid.
fn first(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        let got = s(v, k);
        if !got.is_empty() {
            return got;
        }
    }
    String::new()
}

const SESSION_KEYS: &[&str] = &["session_id", "conversationId", "conversation_id"];
const TRANSCRIPT_KEYS: &[&str] = &["transcript_path", "transcriptPath"];

/// The working directory a payload reports, if it reports one.
///
/// agy sends `workspacePaths`, an array — an agy session can have several
/// directories added to it. The first is the one it was started in, which is
/// what corresponds to every other agent's single cwd.
fn cwd_of(v: &Value) -> String {
    let direct = first(v, &["cwd", "workingDirectory"]);
    if !direct.is_empty() {
        return direct;
    }
    v.get("workspacePaths")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
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

fn append_legacy(path: &Path, line: &str) {
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

fn enqueue(spool: &Path, channel: &str, payload: Value) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let sequence = SPOOL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let id = format!(
        "{:020}-{:09}-{:010}-{sequence:06}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id()
    );
    let record = json!({
        "version": 1,
        "id": id,
        "channel": channel,
        "queued_at": now.as_secs(),
        "payload": payload,
    });
    let text = record.to_string();
    if text.len() > ATOMIC_LIMIT || std::fs::create_dir_all(spool).is_err() {
        return false;
    }
    let tmp = spool.join(format!("{id}.tmp"));
    let ready = spool.join(format!("{id}.ready"));
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
    else {
        return false;
    };
    if file.write_all(text.as_bytes()).is_err() {
        return false;
    }
    drop(file);
    std::fs::rename(tmp, ready).is_ok()
}

fn publish(spool: &Path, channel: &str, legacy: &Path, payload: Value) {
    if !enqueue(spool, channel, payload.clone()) {
        append_legacy(legacy, &payload.to_string());
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

/// The process a hook ran under.
#[derive(Debug, PartialEq, Eq)]
enum Agent {
    /// The agent process itself, one per session.
    Own(u32),
    /// A process serving many sessions at once: codex's app-server, which
    /// since 0.159 runs every TUI's turns (and so its hooks) in one shared
    /// daemon. Its pid names no session, so it is recorded as the host and
    /// the session is bound to a client later, by `query`.
    Host(u32),
    Unknown,
}

/// Walk up to the agent process itself. Recording its pid, paired with its
/// start time, is what lets the observer decide staleness exactly — the mapping
/// holds while that exact process lives — rather than with a timeout.
fn agent_pid() -> Agent {
    let mut pid = std::os::unix::process::parent_id();
    let mut ancestry = Vec::new();
    for _ in 0..8 {
        let c = cmdline(pid);
        if c.is_empty() {
            break;
        }
        ancestry.push((pid, c));
        match ppid_of(pid) {
            Some(p) if p != 0 && p != pid => pid = p,
            _ => break,
        }
    }
    classify(&ancestry)
}

/// The first agent in a hook's ancestry, nearest first.
///
/// A host ends the walk rather than being stepped over. Whatever is above
/// the daemon is the TUI that happened to start it, which is one client of
/// many and usually not the one whose turn this is (bn-3c9).
fn classify(ancestry: &[(u32, String)]) -> Agent {
    for (pid, c) in ancestry {
        let names_agent = ["claude", "codex", "opencode", "agy"]
            .iter()
            .any(|n| c.contains(n));
        // Skip our own command line, which names an agent only because this
        // binary is invoked from an agent's hook configuration.
        if !names_agent || c.contains("agentbus") {
            continue;
        }
        if c.contains("codex") && c.split(' ').any(|arg| arg == "app-server") {
            return Agent::Host(*pid);
        }
        return Agent::Own(*pid);
    }
    Agent::Unknown
}

fn register_session(p: &Value, register: &Path, inbox: &Path, spool: &Path) {
    // A subagent shares its parent's pane and must not register as a session of
    // its own; it already appears nested under the parent.
    if !s(p, "agent_id").is_empty() {
        return;
    }
    let session = first(p, SESSION_KEYS);
    if session.is_empty() {
        return;
    }
    // Identity a transcript does not carry. agy's records name neither the
    // working directory nor the model, and both are on every hook payload, so
    // reporting them here is the difference between a named session and an
    // anonymous one from its first prompt rather than its first finished turn.
    let (cwd, model) = (cwd_of(p), first(p, &["modelName", "model"]));
    if !cwd.is_empty() || !model.is_empty() {
        publish(
            spool,
            "inbox",
            inbox,
            json!({
                "kind": "session",
                "session": session,
                "cwd": cwd,
                "model": model,
            }),
        );
    }
    // A record is written even with no pane to put in it. Most of what this
    // carries has nothing to do with a multiplexer: pid and start time are an
    // exact process identity — the thing that lets a supervisor say "this
    // session is the agent I spawned" instead of guessing from cwd and timing —
    // and the transcript path is the only route to a session's subagent
    // sidecars. Returning early on an unrecognised host threw all of that away
    // to say nothing more than "I do not know where this is".
    let agent = agent_pid();
    // A host's environment, and so its pane, is inherited from whichever TUI
    // started it, not from the one whose session this is.
    let (mux, mux_session, pane) = match agent {
        Agent::Host(_) => Default::default(),
        _ => location(),
    };
    let mut line = json!({
        "session_id": session,
        "transcript": first(p, TRANSCRIPT_KEYS),
        "mux": mux,
        "mux_session": mux_session,
        "pane": pane,
    });
    let fields = line.as_object_mut().expect("a JSON object");
    match agent {
        Agent::Own(pid) => {
            fields.insert("pid".into(), json!(pid));
            fields.insert("starttime".into(), json!(starttime(pid)));
        }
        Agent::Host(pid) => {
            fields.insert("pid".into(), json!(0));
            fields.insert("starttime".into(), json!(0));
            fields.insert("host_pid".into(), json!(pid));
            fields.insert("host_starttime".into(), json!(starttime(pid)));
            // When this was, in the unit of a process start time. A client
            // that started after it cannot own the session, which is how
            // `query` tells a TUI's session from an older one in the same
            // directory.
            fields.insert(
                "registered_tick".into(),
                json!(starttime(std::process::id())),
            );
        }
        Agent::Unknown => {
            fields.insert("pid".into(), json!(0));
            fields.insert("starttime".into(), json!(0));
        }
    }
    publish(spool, "register", register, line);
}

/// The end of a turn, for a host that writes no record of one.
///
/// agy is the first: its transcript has no turn-end step, and the boundary is
/// not derivable from what it does write — a `PLANNER_RESPONSE` bearing prose
/// and no tool calls, which is agy's own `NO_TOOL_CALL` stop reason, occurs
/// several times per turn as the model narrates between tool batches. Measured
/// on a real conversation: 13 of them across 7 turns.
///
/// The answer is deliberately not carried here. It would have to survive
/// `ATOMIC_LIMIT`, and an over-long line is dropped whole and without complaint,
/// which would lose precisely the answers worth reading. The transcript path
/// goes instead and the observer reads it — it is already tailing that file.
fn turn_end(p: &Value, inbox: &Path, spool: &Path) {
    let session = first(p, SESSION_KEYS);
    if session.is_empty() {
        return;
    }
    let line = json!({
        "kind": "turn_end",
        "session": session,
        "transcript": first(p, TRANSCRIPT_KEYS),
        "cwd": cwd_of(p),
        "model": first(p, &["modelName", "model"]),
    });
    publish(spool, "inbox", inbox, line);
}

fn subagent(p: &Value, phase: Option<&str>, inbox: &Path, spool: &Path) {
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
    publish(spool, "inbox", inbox, line);
}

/// Report from an agent whose state cannot be read off disk — currently
/// OpenCode, whose plugin API sees transitions that reach no transcript.
fn state(p: &Value, st: Option<&str>, detail: Option<&String>, inbox: &Path, spool: &Path) {
    let Some(st) = st else { return };
    // A host's pane is not this session's; see `register_session`.
    let (pid, (mux, mux_session, pane)) = match agent_pid() {
        Agent::Own(pid) => (pid, location()),
        Agent::Host(_) => (0, Default::default()),
        Agent::Unknown => (0, location()),
    };
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
    publish(spool, "inbox", inbox, line);
}

pub fn default_inbox(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox.jsonl")
}

pub fn default_spool(state_dir: &Path) -> PathBuf {
    state_dir.join("hook-spool")
}

/// Move ready spool records into observer-owned journals, deduplicating the
/// crash window between durable append and spool acknowledgement.
pub fn drain_to_journals(spool: &Path, register: &Path, inbox: &Path) -> bool {
    let mut ready: Vec<PathBuf> = std::fs::read_dir(spool)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "ready"))
                .collect()
        })
        .unwrap_or_default();
    ready.sort();
    if ready.is_empty() {
        return false;
    }
    let mut seen = spool_ids(register);
    seen.extend(spool_ids(inbox));
    let mut register_lines = Vec::new();
    let mut inbox_lines = Vec::new();
    let mut register_files = Vec::new();
    let mut inbox_files = Vec::new();
    let mut duplicate_files = Vec::new();
    for path in ready {
        let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            continue;
        };
        let Some(id) = record.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.contains(id) {
            duplicate_files.push(path);
            continue;
        }
        let channel = record.get("channel").and_then(Value::as_str).unwrap_or("");
        let Some(mut payload) = record.get("payload").cloned() else {
            continue;
        };
        let Some(object) = payload.as_object_mut() else {
            continue;
        };
        object.insert("_spool_id".into(), json!(id));
        object.insert(
            "_spooled_at".into(),
            record.get("queued_at").cloned().unwrap_or(json!(0)),
        );
        match channel {
            "register" => {
                register_lines.push(payload.to_string());
                register_files.push(path);
            }
            "inbox" => {
                inbox_lines.push(payload.to_string());
                inbox_files.push(path);
            }
            _ => {}
        }
        seen.insert(id.to_string());
    }
    let register_ok = append_lines(register, &register_lines);
    let inbox_ok = append_lines(inbox, &inbox_lines);
    if register_ok {
        acknowledge(&register_files);
    }
    if inbox_ok {
        acknowledge(&inbox_files);
    }
    acknowledge(&duplicate_files);
    (register_ok && !register_lines.is_empty()) || (inbox_ok && !inbox_lines.is_empty())
}

fn append_lines(path: &Path, lines: &[String]) -> bool {
    if lines.is_empty() {
        return true;
    }
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return false;
    };
    let mut text = lines.join("\n");
    text.push('\n');
    file.write_all(text.as_bytes()).is_ok() && file.sync_data().is_ok()
}

fn acknowledge(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn spool_ids(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter_map(|value| {
                    value
                        .get("_spool_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Compact the observer-owned inbox after its current cursor has consumed it.
pub fn compact_inbox(path: &Path, now: u64) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let cutoff = now.saturating_sub(JOURNAL_MAX_AGE_SECS);
    let mut lines: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let value = serde_json::from_str::<Value>(line).ok()?;
            let queued = value.get("_spooled_at").and_then(Value::as_u64);
            if queued.is_some_and(|timestamp| timestamp < cutoff) {
                return None;
            }
            Some(value.to_string())
        })
        .collect();
    if lines.len() > JOURNAL_MAX_RECORDS {
        lines.drain(..lines.len() - JOURNAL_MAX_RECORDS);
    }
    while lines.len() > 1
        && lines.iter().map(|line| line.len() + 1).sum::<usize>() > JOURNAL_MAX_BYTES
    {
        lines.remove(0);
    }
    let mut compacted = lines.join("\n");
    if !compacted.is_empty() {
        compacted.push('\n');
    }
    if compacted == text {
        return false;
    }
    write_atomic(path, &compacted)
}

/// Bound abandoned ready/temporary spool files after attempting a normal drain.
/// Only regular files in the agentbus naming namespace are candidates.
pub fn prune_spool(spool: &Path, now: u64) -> usize {
    let cutoff = now.saturating_sub(JOURNAL_MAX_AGE_SECS);
    let mut files: Vec<(PathBuf, u64, u64)> = std::fs::read_dir(spool)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    let extension = path.extension().and_then(|value| value.to_str());
                    if !matches!(extension, Some("ready" | "tmp")) {
                        return None;
                    }
                    let file_type = entry.file_type().ok()?;
                    if !file_type.is_file() || file_type.is_symlink() {
                        return None;
                    }
                    let metadata = entry.metadata().ok()?;
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0);
                    Some((path, metadata.len(), modified))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by_key(|(_, _, modified)| *modified);
    let mut removed = 0;
    for (path, _, modified) in &files {
        if *modified < cutoff && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    files.retain(|(path, _, _)| path.exists());
    let mut total = files.iter().map(|(_, bytes, _)| *bytes).sum::<u64>();
    for (path, bytes, _) in files {
        if total <= JOURNAL_MAX_BYTES as u64 {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(bytes);
            removed += 1;
        }
    }
    removed
}

fn write_atomic(path: &Path, text: &str) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, text).is_ok() && std::fs::rename(tmp, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agentbus-hook-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            dir.join("spool"),
            dir.join("register.jsonl"),
            dir.join("inbox.jsonl"),
        )
    }

    fn ancestry(lines: &[&str]) -> Vec<(u32, String)> {
        lines
            .iter()
            .enumerate()
            .map(|(i, c)| (100 + i as u32, c.to_string()))
            .collect()
    }

    #[test]
    fn a_hook_under_codex_itself_registers_that_process() {
        let chain = ancestry(&[
            "/bin/sh -c /home/u/.local/bin/agentbus hook register",
            "/home/u/.codex/bin/codex --sandbox read-only",
            "vessel server",
        ]);
        assert_eq!(classify(&chain), Agent::Own(101));
        assert_eq!(classify(&ancestry(&["claude --resume"])), Agent::Own(100));
        assert_eq!(classify(&ancestry(&["bash", "init"])), Agent::Unknown);
    }

    /// bn-3c9. A hook run by codex's shared app-server registers it as the
    /// host, and never the TUI that happened to start it.
    #[test]
    fn a_hook_under_the_shared_app_server_registers_it_as_host() {
        let daemon = "/home/u/.codex/packages/standalone/releases/0.159.2/bin/codex \
                      app-server --listen unix:// --managed-daemon";
        assert_eq!(
            classify(&ancestry(&[
                "/bin/sh -c agentbus hook register",
                daemon,
                "systemd --user"
            ])),
            Agent::Host(101)
        );
        assert_eq!(
            classify(&ancestry(&[daemon, "codex --model x", "vessel server"])),
            Agent::Host(100)
        );
        // Named `app-server` only as part of a longer word, or not by codex.
        assert_eq!(
            classify(&ancestry(&["codex --profile my-app-server"])),
            Agent::Own(100)
        );
    }

    #[test]
    fn concurrent_hook_records_do_not_overwrite_each_other() {
        let (spool, register, inbox) = fixture("concurrent");
        let mut workers = Vec::new();
        for n in 0..16 {
            let spool = spool.clone();
            workers.push(std::thread::spawn(move || {
                assert!(enqueue(
                    &spool,
                    "inbox",
                    json!({"kind":"state", "session":format!("s-{n}")})
                ));
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(std::fs::read_dir(&spool).unwrap().count(), 16);
        assert!(drain_to_journals(&spool, &register, &inbox));
        assert_eq!(std::fs::read_to_string(inbox).unwrap().lines().count(), 16);
        assert_eq!(std::fs::read_dir(spool).unwrap().count(), 0);
    }

    #[test]
    fn replayed_spool_record_is_deduplicated_after_append_before_ack() {
        let (spool, register, inbox) = fixture("dedup");
        assert!(enqueue(
            &spool,
            "inbox",
            json!({"kind":"state", "session":"s"})
        ));
        let original = std::fs::read_dir(&spool)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let record = std::fs::read_to_string(&original).unwrap();
        assert!(drain_to_journals(&spool, &register, &inbox));
        std::fs::write(spool.join("replayed.ready"), record).unwrap();
        assert!(!drain_to_journals(&spool, &register, &inbox));
        assert_eq!(std::fs::read_to_string(inbox).unwrap().lines().count(), 1);
        assert_eq!(std::fs::read_dir(spool).unwrap().count(), 0);
    }

    #[test]
    fn inbox_compaction_expires_timestamped_history() {
        let (_, _, inbox) = fixture("compact");
        std::fs::write(
            &inbox,
            "{\"kind\":\"state\",\"session\":\"old\",\"_spooled_at\":1}\n{\"kind\":\"state\",\"session\":\"new\",\"_spooled_at\":1000}\n",
        )
        .unwrap();
        assert!(compact_inbox(&inbox, 1000 + JOURNAL_MAX_AGE_SECS));
        let text = std::fs::read_to_string(inbox).unwrap();
        assert!(!text.contains("old"));
        assert!(text.contains("new"));
    }
}
