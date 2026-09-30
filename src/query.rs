//! The reading side: answering questions instead of publishing a file.
//!
//! agentbus published state but nothing consumed it on a caller's behalf, so
//! every subscriber reimplemented the same four things — find the snapshot,
//! parse its schema, resolve an identity to a session, and keep up as both
//! change. In the first supervisor written against it that was 40% of the
//! script, and it is also where the breakage landed: when the session shape
//! changed the consumer did not error, it silently bound nothing and every wait
//! reported a timeout while the agent sat there having already answered.
//!
//! Behind a query verb the snapshot stops being a public API. That is the point
//! of this module: the file becomes an internal transport, and the schema
//! version becomes a detail of it rather than a contract with every subscriber.

use crate::register;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Print a line, treating a closed stdout as the end rather than an error.
///
/// `println!` panics on EPIPE, so `agentbus sessions | head` died with a
/// backtrace instead of stopping — the one thing a listing verb is guaranteed
/// to be piped into. Returns false once the reader has gone, so callers can
/// stop generating output nobody will read.
pub fn line(out: &mut impl Write, s: &str) -> bool {
    writeln!(out, "{s}").is_ok()
}

/// Where the running observer publishes.
///
/// Consumers used to hardcode this, along with a precedence rule between the
/// state dir and whatever directory the service was pointed at. That is the
/// publisher's business, not theirs — but the publisher is a separate process,
/// so it has to say so somewhere. It writes this record on startup, and the
/// reading verbs follow it.
///
/// Deliberately not a search of likely locations: a stale snapshot in a
/// forgotten directory is indistinguishable from a live one to anything except
/// the process that wrote it.
#[derive(Debug, Clone)]
pub struct Locations {
    pub snapshot: PathBuf,
    pub log: PathBuf,
    pub register: PathBuf,
    pub completions: PathBuf,
}

pub fn publisher_path(state_dir: &Path) -> PathBuf {
    state_dir.join("publisher.json")
}

/// Record where this observer publishes, so readers need not guess.
///
/// Written on every startup rather than once: the paths come from the command
/// line, so a service edited to publish elsewhere would otherwise leave a
/// record pointing at a file nobody updates any more.
pub fn publish_locations(state_dir: &Path, loc: &Locations) {
    let _ = std::fs::create_dir_all(state_dir);
    let line = json!({
        "snapshot": loc.snapshot.to_string_lossy(),
        "log": loc.log.to_string_lossy(),
        "register": loc.register.to_string_lossy(),
        "completions": loc.completions.to_string_lossy(),
        "pid": std::process::id(),
    });
    let path = publisher_path(state_dir);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, line.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Resolve where to read from: what the caller pinned, else what the running
/// observer said, else the defaults.
///
/// `pinned` is whatever the caller passed explicitly; it always wins, so a test
/// or a second observer can be pointed at without touching the live one.
pub fn resolve(state_dir: &Path, pinned: &Locations, pinned_snapshot: bool) -> Locations {
    let mut out = pinned.clone();
    let published = std::fs::read_to_string(publisher_path(state_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    if let Some(v) = published {
        let get = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        // Only fill in what the caller did not pin. The snapshot is the one that
        // needs the flag to distinguish "pinned" from "left at its default",
        // since its default is a real path rather than empty.
        if !pinned_snapshot {
            if let Some(p) = get("snapshot") {
                out.snapshot = p;
            }
        }
        if let Some(p) = get("log") {
            out.log = p;
        }
        if let Some(p) = get("register") {
            out.register = p;
        }
        if let Some(p) = get("completions") {
            out.completions = p;
        }
    }
    out
}

/// One session as the query verbs report it: what the snapshot knows about its
/// state, plus what the register knows about where and what process it is.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    /// The snapshot's object for this session, passed through whole.
    pub state: Value,
    pub pid: u64,
    /// Nonzero only with `pid`, after exact pid/start-time verification.
    pub starttime: u64,
    pub transcript: String,
    /// The shared process serving this session, when a hook ran under one
    /// rather than under the agent (codex's app-server). Nonzero only while
    /// that exact process lives, and then `pid` is 0: the host names no
    /// session, so this one is found by `--pid` only through `Client`.
    pub host_pid: u64,
    /// When the session last registered, in clock ticks since boot.
    pub registered_tick: u64,
}

/// How confidently this record describes a process that exists right now.
///
/// Transcript discovery deliberately retains recent history, and hook reports
/// can outlive the process that emitted them. Neither is evidence of liveness.
/// Only a registration whose pid *and start time* still match is exact enough
/// to call live; everything else remains queryable but is explicitly marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Verified,
    /// Served by a live shared host. The session can still take a turn, but
    /// no process of its own proves anyone is attached to it.
    Hosted,
    Unverified,
}

impl Presence {
    pub fn label(self) -> &'static str {
        match self {
            Presence::Verified => "verified",
            Presence::Hosted => "hosted",
            Presence::Unverified => "unverified",
        }
    }
}

impl Session {
    pub fn presence(&self) -> Presence {
        if self.pid > 0 && self.starttime > 0 {
            Presence::Verified
        } else if self.host_pid > 0 {
            Presence::Hosted
        } else {
            Presence::Unverified
        }
    }

    pub fn text(&self, key: &str) -> &str {
        self.state.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }

    pub fn number(&self, key: &str) -> u64 {
        self.state.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    }

    pub fn pointer_text(&self, pointer: &str) -> &str {
        self.state
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    pub fn pointer_number(&self, pointer: &str) -> u64 {
        self.state
            .pointer(pointer)
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }
}

/// Read the published snapshot. `None` means there is none to read — the
/// observer is not running, or has not published yet.
pub fn load_snapshot(path: &Path) -> Option<Value> {
    let txt = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&txt).ok()
}

/// Every session the snapshot describes, joined to its registration.
pub fn all(loc: &Locations) -> Option<Vec<Session>> {
    let snap = load_snapshot(&loc.snapshot)?;
    let regs = register::load(&loc.register);
    let sessions = snap.get("sessions")?.as_array()?;
    Some(
        sessions
            .iter()
            .map(|s| {
                let id = s
                    .get("session")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                let reg = regs.get(&id);
                let live = reg.map(register::exactly_live).unwrap_or(false);
                let hosted = reg.filter(|p| register::host_live(p));
                Session {
                    id,
                    state: s.clone(),
                    // A registration whose process is gone says nothing useful
                    // about a pid, and reporting one that has been recycled into
                    // an unrelated program is worse than reporting none.
                    pid: reg.filter(|_| live).map(|p| p.pid).unwrap_or(0),
                    starttime: reg.filter(|_| live).map(|p| p.starttime).unwrap_or(0),
                    transcript: reg.map(|p| p.transcript.clone()).unwrap_or_default(),
                    host_pid: hosted.map(|p| p.host_pid).unwrap_or(0),
                    registered_tick: hosted.map(|p| p.registered_tick).unwrap_or(0),
                }
            })
            .collect(),
    )
}

/// /proc/<pid>/stat's comm field can contain spaces and parens, so fields
/// cannot be counted from the left; everything after the last ')' is fixed.
fn ppid_of(pid: u64) -> Option<u64> {
    let txt = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = txt.rsplit_once(')')?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// How far up a process tree to look for the queried pid. A supervisor is
/// normally one or two levels above the agent; the bound exists only so a
/// /proc that lies about ppid cannot spin here.
const MAX_DEPTH: u32 = 16;

/// Is `pid` the process `ancestor`, or a descendant of it?
///
/// Matching descendants rather than the pid alone is the whole reason this is
/// agentbus's job. agentbus registers the *agent* process, which is not always
/// the one a supervisor spawned: codex runs behind a node shim, so a supervisor
/// holds the shim's pid while the registration holds the real binary one level
/// below. For claude the two coincide, which is exactly what makes this an easy
/// bug to ship — it works until the day it is pointed at codex.
pub fn is_self_or_descendant(pid: u64, ancestor: u64) -> bool {
    if pid == 0 || ancestor == 0 {
        return false;
    }
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        if cur == ancestor {
            return true;
        }
        match ppid_of(cur) {
            // Stop at the top of the tree rather than walking off it: init's
            // ppid reads as 0, and a self-parent would spin. Asking about pid 1
            // still matches, since the check above happens first — everything
            // genuinely is its descendant.
            Some(p) if p != 0 && p != cur && cur != 1 => cur = p,
            _ => return false,
        }
    }
    false
}

/// A codex client at or below a queried pid: the TUI a supervisor spawned,
/// or the binary behind the shim it spawned.
///
/// Since codex 0.159 a client's turns, and so its hooks, run in the shared
/// app-server, and nothing a hook can see names the client (bn-3c9). What
/// ties one to its session is where it runs and when: the session's working
/// directory is the client's, and it registered after the client started. A
/// supervisor that gives each agent a directory of its own, which concurrent
/// agents need anyway, gets an exact binding; two clients in one directory
/// are ambiguous and reported as such, never guessed between.
#[derive(Debug)]
struct Client {
    pid: u64,
    /// `None` when /proc will not say, which makes the binding impossible
    /// and is reported rather than waited on.
    cwd: Option<PathBuf>,
    starttime: u64,
}

impl Client {
    fn serves(&self, s: &Session) -> bool {
        let Some(cwd) = &self.cwd else {
            return false;
        };
        s.host_pid > 0
            && s.registered_tick >= self.starttime
            && same_dir(field(s, "cwd"), &cwd.to_string_lossy())
    }
}

/// Is this command line a codex client, as opposed to the shared host or one
/// of codex's own helpers (`codex-code-mode-host`, plugin servers)? The real
/// binary is `codex` itself; the npm shim is an interpreter running a script
/// named `codex`.
fn is_codex_client(args: &[&str]) -> bool {
    let named = |i: usize| {
        args.get(i)
            .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "codex"))
    };
    (named(0) || named(1)) && !args.contains(&"app-server")
}

fn cmdline_args(pid: u64) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// Every codex client that is `ancestor` or a descendant of it.
fn clients_under(ancestor: u64) -> Vec<Client> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(cmdline) = cmdline_args(pid) else {
            continue;
        };
        let args: Vec<&str> = cmdline.split('\0').filter(|a| !a.is_empty()).collect();
        if !is_codex_client(&args) || !is_self_or_descendant(pid, ancestor) {
            continue;
        }
        let Some(starttime) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|txt| register::parse_stat(&txt))
            .map(|(_, starttime)| starttime)
        else {
            continue;
        };
        out.push(Client {
            pid,
            cwd: std::fs::read_link(format!("/proc/{pid}/cwd")).ok(),
            starttime,
        });
    }
    out
}

/// Same directory, tolerating the trailing-slash and symlink differences
/// between what an agent recorded and what a caller typed.
fn same_dir(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let norm = |s: &str| s.trim_end_matches('/').to_string();
    if norm(a) == norm(b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[derive(Default)]
pub struct Filter {
    pub pid: Option<u64>,
    pub session: Option<String>,
    pub cwd: Option<String>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.pid.is_none() && self.session.is_none() && self.cwd.is_none()
    }

    fn matches(&self, s: &Session, clients: &[Client]) -> bool {
        if let Some(want) = &self.session {
            // Prefix rather than equality would be wrong here: codex session ids
            // are UUIDv7, so two created seconds apart share a long prefix and a
            // truncated id makes distinct agents look identical.
            if &s.id != want {
                return false;
            }
        }
        if let Some(want) = self.pid {
            if !is_self_or_descendant(s.pid, want) && !clients.iter().any(|c| c.serves(s)) {
                return false;
            }
        }
        if let Some(want) = &self.cwd {
            let cwd = s.state.get("cwd").and_then(|x| x.as_str()).unwrap_or("");
            if !same_dir(cwd, want) {
                return false;
            }
        }
        true
    }
}

/// The sessions `f` matches, and whatever made `--pid` unanswerable if it
/// matched none: a lookup that cannot succeed says so at once rather than
/// leaving a caller to wait out its timeout.
fn select(sessions: Vec<Session>, f: &Filter) -> (Vec<Session>, Option<String>) {
    // Reading /proc for clients is only worth it when some session is hosted.
    let clients = match f.pid {
        Some(want) if sessions.iter().any(|s| s.host_pid > 0) => clients_under(want),
        _ => Vec::new(),
    };
    let hosts: Vec<u64> = sessions
        .iter()
        .map(|s| s.host_pid)
        .filter(|&h| h > 0)
        .collect();
    let found: Vec<Session> = sessions
        .into_iter()
        .filter(|s| f.matches(s, &clients))
        .collect();
    let why = match f.pid {
        Some(want) if found.is_empty() => unbindable(want, &hosts, &clients),
        _ => None,
    };
    (found, why)
}

fn unbindable(want: u64, hosts: &[u64], clients: &[Client]) -> Option<String> {
    if hosts.contains(&want) {
        return Some(format!(
            "pid {want} is codex's shared app-server, which serves every codex \
             session; pass the pid of the codex client, or use --session or --cwd"
        ));
    }
    let blind = clients.iter().find(|c| c.cwd.is_none())?;
    Some(format!(
        "codex client {} under pid {want} runs its sessions in the shared \
         app-server, so --pid finds them by the client's directory, and that \
         cannot be read; use --session or --cwd",
        blind.pid
    ))
}

/// Exactly one session, for the verbs that act on a single one.
///
/// Ambiguity is an error rather than a pick: a supervisor that waits on the
/// wrong one of two agents sharing a directory waits for the wrong answer and
/// has no way to tell.
pub enum Resolved {
    One(Box<Session>),
    None,
    Many(usize),
    /// Nothing matched, and nothing will: the reason why.
    Unbindable(String),
}

pub fn resolve_one(loc: &Locations, f: &Filter) -> Option<Resolved> {
    let (mut found, why) = select(all(loc)?, f);
    Some(match found.len() {
        0 => why.map_or(Resolved::None, Resolved::Unbindable),
        1 => Resolved::One(Box::new(found.remove(0))),
        n => Resolved::Many(n),
    })
}

fn field<'a>(s: &'a Session, k: &str) -> &'a str {
    s.state.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

/// `agentbus sessions` — print matching sessions from current state.
pub fn run(args: &[String], loc: &Locations) -> i32 {
    let mut f = Filter::default();
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        let next = args.get(i + 1);
        match args[i].as_str() {
            "--pid" => f.pid = next.and_then(|v| v.parse().ok()),
            "--session" => f.session = next.cloned(),
            "--cwd" => f.cwd = next.cloned(),
            "--json" => as_json = true,
            _ => {}
        }
        i += 1;
    }

    let Some(sessions) = all(loc) else {
        eprintln!(
            "agentbus: no snapshot at {}; is the observer running?",
            loc.snapshot.display()
        );
        return 1;
    };
    let (found, why) = select(sessions, &f);
    if let Some(why) = why {
        eprintln!("agentbus: {why}");
        return 1;
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if as_json {
        let arr: Vec<Value> = found
            .iter()
            .map(|s| {
                let mut v = s.state.clone();
                if let Some(o) = v.as_object_mut() {
                    // Neither is in the snapshot: they come from the register,
                    // and they are what a supervisor needs to say "this session
                    // is the agent I spawned" rather than inferring it.
                    o.insert("pid".into(), json!(s.pid));
                    o.insert("transcript".into(), json!(s.transcript));
                    if s.host_pid > 0 {
                        o.insert("host_pid".into(), json!(s.host_pid));
                    }
                }
                v
            })
            .collect();
        line(
            &mut out,
            &serde_json::to_string(&json!(arr)).unwrap_or_default(),
        );
        return 0;
    }

    for s in &found {
        let loc_str = {
            let l = s.state.get("location");
            let g = |k: &str| {
                l.and_then(|x| x.get(k))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
            };
            let (mux, sess, pane) = (g("mux"), g("session"), g("pane"));
            if pane.is_empty() {
                String::new()
            } else {
                format!("{mux}/{sess}/{pane}")
            }
        };
        let row = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            s.id,
            field(s, "state"),
            field(s, "source"),
            s.pid,
            loc_str,
            field(s, "cwd"),
            field(s, "label"),
        );
        if !line(&mut out, &row) {
            break;
        }
    }
    0
}

/// Human-readable reason a single-session lookup failed, for the verbs that
/// need one. Kept here so `sessions` and `wait` word it the same way.
pub fn describe_miss(f: &Filter, r: &Resolved) -> String {
    let what = match (&f.session, f.pid, &f.cwd) {
        (Some(s), _, _) => format!("session {s}"),
        (_, Some(p), _) => format!("pid {p} or any descendant"),
        (_, _, Some(c)) => format!("cwd {c}"),
        _ => "any session".to_string(),
    };
    match r {
        Resolved::None => format!("no session matched {what}"),
        Resolved::Many(n) => {
            format!("{n} sessions matched {what}; narrow it with --session")
        }
        Resolved::Unbindable(why) => why.clone(),
        Resolved::One(_) => String::new(),
    }
}

/// Sessions keyed by id, for callers folding the snapshot themselves.
pub fn by_id(loc: &Locations) -> BTreeMap<String, Session> {
    all(loc)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_is_its_own_ancestor() {
        let me = std::process::id() as u64;
        assert!(is_self_or_descendant(me, me));
    }

    #[test]
    fn finds_a_real_ancestor() {
        let me = std::process::id() as u64;
        let parent = ppid_of(me).expect("a test process has a parent");
        assert!(is_self_or_descendant(me, parent));
    }

    /// The direction matters: a supervisor holds the ancestor and asks about the
    /// agent below it, never the reverse.
    #[test]
    fn is_not_symmetric() {
        let me = std::process::id() as u64;
        let parent = ppid_of(me).expect("a test process has a parent");
        assert!(!is_self_or_descendant(parent, me));
    }

    #[test]
    fn zero_matches_nothing() {
        let me = std::process::id() as u64;
        assert!(!is_self_or_descendant(0, me));
        assert!(!is_self_or_descendant(me, 0));
    }

    #[test]
    fn codex_clients_are_the_binary_or_its_shim_and_never_the_host() {
        assert!(is_codex_client(&["codex", "--sandbox", "read-only"]));
        assert!(is_codex_client(&["/home/u/.codex/bin/codex"]));
        assert!(is_codex_client(&[
            "node",
            "/usr/lib/node_modules/.bin/codex"
        ]));
        assert!(!is_codex_client(&[
            "/x/codex",
            "app-server",
            "--managed-daemon"
        ]));
        assert!(!is_codex_client(&["/x/codex-code-mode-host"]));
        assert!(!is_codex_client(&["node", "/x/.codex/plugins/server.mjs"]));
        assert!(!is_codex_client(&[]));
    }

    #[test]
    fn trailing_slash_is_the_same_dir() {
        assert!(same_dir("/home/x", "/home/x/"));
        assert!(!same_dir("/home/x", "/home/y"));
        assert!(!same_dir("", "/home/x"));
    }

    #[test]
    fn only_a_live_registration_is_verified() {
        let mut s = Session {
            id: "s".into(),
            state: json!({}),
            pid: 0,
            starttime: 0,
            transcript: String::new(),
            ..Default::default()
        };
        assert_eq!(s.presence(), Presence::Unverified);
        s.pid = 42;
        s.starttime = 7;
        assert_eq!(s.presence(), Presence::Verified);
    }
}
