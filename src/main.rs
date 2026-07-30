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
mod tail;

use discover::Source;
use event::{Event, Snapshot};
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

OPTIONS:
    --within <MINS>   How recently a transcript must have changed (default 30)
    --interval <MS>   Poll interval (default 300)
    --snapshot <PATH> Where to write state   (default: zellij tmp, else state dir)
    --log <PATH>      Where to append events (default: state dir)
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
    publish: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Opts {
        let mut o = Opts {
            within: Duration::from_secs(30 * 60),
            interval: Duration::from_millis(300),
            snapshot: default_snapshot_path(),
            log: state_dir().join("events.jsonl"),
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
    let stdout = std::io::stdout();

    // On a one-shot scan, replay each file from the start so the snapshot
    // reflects the whole session. When following, start at the end — otherwise
    // the first tick replays thousands of historical lines as if they were new.
    let from_start = !follow;
    let mut first = true;

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
        tails.drop_untracked(&live);
        streams.retain(|p, _| live.contains(p));

        let mut batch: Vec<Event> = Vec::new();
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

        if opts.publish && !batch.is_empty() {
            append_log(&opts.log, &batch);
        }
        if opts.publish && (!batch.is_empty() || first) {
            write_snapshot(&opts.snapshot, &snap);
        }
        if !print_snapshot {
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
fn write_snapshot(path: &std::path::Path, snap: &Snapshot) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    let Ok(txt) = serde_json::to_string(&snap.to_json()) else {
        return;
    };
    if std::fs::write(&tmp, txt).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
