//! agentbus — observe coding agents, publish normalised events.
//!
//! Prototype of the observer half of the herd split. It tails agent transcripts
//! on disk, normalises them into one vocabulary, and publishes two things:
//!
//!   - an append-only event log   (subscribers tail it; replayable, `tail -f`able)
//!   - a snapshot of current state (subscribers that only want "what is true now")
//!
//! Why transcripts rather than hooks: they need no agent cooperation, carry far
//! more (tools, tokens, titles, turn boundaries), and have no race — we measured
//! Claude's subagent meta file landing a full second *after* its start hook.
//!
//! Why this is a separate process at all: a zellij plugin is WASI-sandboxed and
//! only preopens /host, /data and /tmp, so it can never read ~/.claude or
//! ~/.codex. That sandbox is the actual reason for the split.
//!
//! What this deliberately does NOT do: decide "blocked". Neither agent records
//! permission prompts to disk — approval is UI state. That signal only exists on
//! screen, so it stays in the plugin.

mod claude;
mod codex;
mod discover;
mod event;
mod hook;
mod register;
mod tail;

use discover::Source;
use event::{Event, Kind, Snapshot};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
agentbus — observe coding agents, publish normalised events

USAGE:
    agentbus watch              Follow live transcripts and publish
    agentbus scan               Fold recent history once, print the snapshot
    agentbus events             Follow and print events to stdout only
    agentbus hook register      Publish this session's pane (from a hook)
    agentbus hook subagent start|stop
    agentbus hook state <state> [detail]

OPTIONS:
    --within <MINS>   How recently a transcript must have changed (default 30)
    --interval <MS>   Poll interval (default 300)
    --snapshot <PATH> Where to write state   (default: zellij tmp, else state dir)
    --log <PATH>      Where to append events (default: state dir)
    --register <PATH> Session->pane registrations (default: state dir)
    --result-ttl <S>  How long a finished subagent's result stays published (90)
    --no-publish      Do not write snapshot or log
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprint!("{USAGE}");
        std::process::exit(2);
    }
    let opts = Opts::parse(&args);
    match args[0].as_str() {
        "hook" => hook::run(&args[1..], &opts.register, &opts.inbox),
        "watch" => run(&opts, true, true),
        "events" => run(&opts, true, false),
        "scan" => run(&opts, false, true),
        "-h" | "--help" | "help" => print!("{USAGE}"),
        other => {
            eprintln!("agentbus: unknown command {other:?}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

struct Opts {
    within: Duration,
    interval: Duration,
    snapshot: PathBuf,
    log: PathBuf,
    register: PathBuf,
    inbox: PathBuf,
    result_ttl: Duration,
    publish: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Opts {
        let mut o = Opts {
            within: Duration::from_secs(30 * 60),
            interval: Duration::from_millis(300),
            snapshot: default_snapshot_path(),
            log: state_dir().join("events.jsonl"),
            register: state_dir().join("register.jsonl"),
            inbox: hook::default_inbox(&state_dir()),
            result_ttl: event::RESULT_TTL,
            publish: true,
        };
        let mut i = 0;
        while i < args.len() {
            let next = args.get(i + 1);
            match args[i].as_str() {
                "--within" => {
                    if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                        o.within = Duration::from_secs(v * 60);
                    }
                }
                "--interval" => {
                    if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                        o.interval = Duration::from_millis(v);
                    }
                }
                "--snapshot" => {
                    if let Some(v) = next {
                        o.snapshot = PathBuf::from(v);
                    }
                }
                "--log" => {
                    if let Some(v) = next {
                        o.log = PathBuf::from(v);
                    }
                }
                "--register" => {
                    if let Some(v) = next {
                        o.register = PathBuf::from(v);
                    }
                }
                "--result-ttl" => {
                    if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                        o.result_ttl = Duration::from_secs(v);
                    }
                }
                "--no-publish" => o.publish = false,
                _ => {}
            }
            i += 1;
        }
        o
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

fn state_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state"))
        .join("agentbus")
}

/// Prefer zellij's sandboxed tmp: a plugin sees that directory as `/tmp`, so a
/// snapshot written to `/tmp/zellij-<uid>/agentbus.json` is readable from inside
/// the WASI sandbox as `/tmp/agentbus.json` — the one channel that reaches herd
/// without any push machinery.
fn default_snapshot_path() -> PathBuf {
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for e in entries.flatten() {
            let p = e.path();
            let is_zellij = p
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("zellij-"))
                .unwrap_or(false);
            if is_zellij && p.is_dir() {
                return p.join("agentbus.json");
            }
        }
    }
    state_dir().join("snapshot.json")
}

/// Per-file context: what we learned about a transcript when we started it.
struct Stream {
    path: PathBuf,
    source: Source,
    /// Session id derived from the filename, used until the stream names itself.
    fallback: String,
    /// Set for Claude subagent transcripts.
    parent: Option<String>,
    agent_id: String,
    agent_type: String,
    description: String,
    /// Last published identity fingerprint, for suppressing restatements.
    last_identity: Option<String>,
}

fn run(opts: &Opts, follow: bool, print_snapshot: bool) {
    let mut tails = tail::MultiTail::default();
    let mut streams: BTreeMap<PathBuf, Stream> = BTreeMap::new();
    let mut snap = Snapshot::default();
    // Pane bindings reported through the inbox by agents with no transcript.
    let mut inbox_panes: BTreeMap<String, register::Pane> = BTreeMap::new();
    let stdout = std::io::stdout();

    // Always replay from the start: the snapshot is a statement of what is
    // true now, and an observer that ignored history would show every already
    // running session as nameless and stateless until it happened to speak.
    //
    // The event log is different — it records things happening — so the
    // backfill pass is folded into state but not published as events.
    let from_start = true;
    let mut first = true;
    // Last text written, so the file is only rewritten when it would differ.
    let mut published = String::new();

    loop {
        // Rediscovery each tick is what makes new sessions appear without
        // restarting; the recency filter keeps it to a handful of files.
        let found = discover::active(opts.within);
        let mut live: Vec<PathBuf> = Vec::new();
        for f in &found {
            live.push(f.path.clone());
            if tails.is_tracked(&f.path) {
                continue;
            }
            tails.track(&f.path, from_start);
            streams.insert(f.path.clone(), new_stream(f));
        }
        live.push(opts.inbox.clone());
        tails.drop_untracked(&live);
        streams.retain(|p, _| live.contains(p));

        // The inbox is tailed like a transcript: agents that cannot be read off
        // disk publish here, and so do reports no transcript carries.
        tails.track(&opts.inbox, true);
        let mut batch: Vec<Event> = Vec::new();
        for line in tails.poll(&opts.inbox) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                batch.extend(inbox_events(&v, &mut inbox_panes));
            }
        }
        for path in &live {
            let Some(st) = streams.get_mut(path) else {
                continue;
            };
            for line in tails.poll(path) {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                for e in events_for(st, &v) {
                    // Identity is restated on every assistant line. Republishing
                    // unchanged cwd/model would make the log mostly noise, and a
                    // log worth tailing is the point of an append-only bus.
                    if let event::Kind::Session { cwd, model, title, .. } = &e.kind {
                        let fingerprint = format!(
                            "{}|{}|{}",
                            cwd.clone().unwrap_or_default(),
                            model.clone().unwrap_or_default(),
                            title.clone().unwrap_or_default()
                        );
                        if st.last_identity.as_deref() == Some(fingerprint.as_str()) {
                            continue;
                        }
                        st.last_identity = Some(fingerprint);
                    }
                    batch.push(e);
                }
            }
        }

        for e in &batch {
            snap.apply(e);
        }

        snap.expire(opts.result_ttl);

        // Registrations are re-read wholesale each pass, then verified against
        // the live process. A closed pane drops its mapping the same tick.
        let mut regs = register::load(&opts.register);
        for (k, v) in &inbox_panes {
            regs.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (session, st) in snap.sessions.iter_mut() {
            match regs.get(session) {
                Some(p) if register::still_true(p) => {
                    st.zellij_session = p.zellij_session.clone();
                    st.pane_id = p.pane_id.clone();
                }
                _ => {
                    st.zellij_session.clear();
                    st.pane_id.clear();
                }
            }
        }

        // `first` is the backfill pass: real history, but not news.
        if opts.publish && !batch.is_empty() && !first {
            append_log(&opts.log, &batch);
        }
        // Publish on any observable difference, not on "did events arrive".
        // Expiry and a registration going stale both change what is true while
        // producing no event, so keying the write off the batch left the file
        // saying things that had stopped being so.
        if opts.publish {
            let txt = serde_json::to_string(&snap.to_json()).unwrap_or_default();
            if txt != published {
                write_snapshot(&opts.snapshot, &txt);
                published = txt;
            }
        }
        if !print_snapshot && !first {
            let mut out = stdout.lock();
            for e in &batch {
                let _ = writeln!(out, "{}", e.to_json());
            }
            let _ = out.flush();
        }
        first = false;

        if !follow {
            break;
        }
        std::thread::sleep(opts.interval);
    }

    if print_snapshot {
        println!(
            "{}",
            serde_json::to_string_pretty(&snap.to_json()).unwrap_or_default()
        );
    }
}

fn new_stream(f: &discover::Found) -> Stream {
    let stem = f
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let is_sub = f.parent_session.is_some();
    let agent_id = stem.trim_start_matches("agent-").to_string();
    let (agent_type, description) = if is_sub {
        claude::subagent_meta(&f.path)
    } else {
        (String::new(), String::new())
    };
    Stream {
        path: f.path.clone(),
        source: f.source,
        // Codex filenames are rollout-<ts>-<uuid>; the uuid is the tail.
        fallback: stem
            .rsplit_once('-')
            .map(|(_, id)| id.to_string())
            .unwrap_or(stem),
        parent: f.parent_session.clone(),
        agent_id,
        agent_type,
        description,
        last_identity: None,
    }
}

fn events_for(st: &mut Stream, v: &serde_json::Value) -> Vec<Event> {
    match st.source {
        Source::Claude => match &st.parent {
            Some(parent) => {
                // The sidecar is written about a second after the subagent
                // starts, so a stream opened at spawn time legitimately has no
                // description yet. Retry until it appears rather than settle.
                if st.description.is_empty() {
                    let (t, d) = claude::subagent_meta(&st.path);
                    if !d.is_empty() {
                        st.agent_type = t;
                        st.description = d;
                    }
                }
                claude::normalize_subagent(
                    v,
                    parent,
                    &st.agent_id,
                    &st.agent_type,
                    &st.description,
                )
            }
            None => claude::normalize(v, &st.fallback),
        },
        Source::Codex => {
            // Only session_meta carries the session id; every later line would
            // otherwise fall back to a filename-derived one and show up as a
            // second, phantom session. Learn it once, then reuse it.
            if let Some(id) = v.pointer("/payload/session_id").and_then(|x| x.as_str()) {
                st.fallback = id.to_string();
            }
            // A Codex subagent's rollout names itself but not its parent, so a
            // nickname is all we can attribute without the parent's stream.
            if let Some(nick) = codex::nickname(v) {
                st.description = nick;
            }
            codex::normalize(v, &st.fallback)
        }
    }
}

fn append_log(path: &std::path::Path, batch: &[Event]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    for e in batch {
        let _ = writeln!(f, "{}", e.to_json());
    }
}

/// Written via a temp file and renamed, so a subscriber polling the path never
/// reads a half-written snapshot. The plugin polls this once a second.
fn write_snapshot(path: &std::path::Path, txt: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, txt).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Translate an inbox record into normalised events.
///
/// These come from agents rather than from disk, so they are reports rather
/// than observations — but they carry exactly what the transcripts cannot: a
/// subagent's completion and result, and the state of an agent that writes no
/// transcript we can read.
fn inbox_events(
    v: &serde_json::Value,
    panes: &mut BTreeMap<String, register::Pane>,
) -> Vec<Event> {
    let g = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let session = g("session");
    if session.is_empty() {
        return Vec::new();
    }
    // A report may be the only thing that knows where this agent lives.
    let pane_id = g("pane_id");
    if !pane_id.is_empty() {
        panes.insert(
            session.clone(),
            register::Pane {
                zellij_session: g("zellij_session"),
                pane_id,
                pid: v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0),
            },
        );
    }
    let mk = |kind: Kind| {
        vec![Event {
            ts: String::new(),
            source: "hook",
            session: session.clone(),
            kind,
        }]
    };
    match g("kind").as_str() {
        "subagent" => {
            let stopped = g("event") == "stop";
            mk(Kind::Subagent {
                id: g("agent_id"),
                // "done" is meaningful for a subagent even though it is not for
                // a session: a stopped subagent is over, not merely between
                // prompts, and it has a final result.
                state: if stopped { "done" } else { "working" },
                agent_type: Some(g("agent_type")),
                description: Some(g("description")),
                result: Some(g("result")),
            })
        }
        "state" => {
            let st = g("state");
            let detail = g("detail");
            mk(Kind::Reported { state: st, detail })
        }
        _ => Vec::new(),
    }
}
