//! agentbus — observe coding agents, publish normalised events.
//!
//! Prototype of the observer half of the herd split. It tails agent transcripts
//! on disk, normalises them into one vocabulary, and publishes two things:
//!
//!   - bounded event generations (subscribers follow the active path with `tail -F`)
//!   - a snapshot of current state (subscribers that only want "what is true now")
//!
//! Why transcripts rather than hooks: they need no agent cooperation, carry far
//! more (tools, tokens, titles, turn boundaries), and have no race — we measured
//! Claude's subagent meta file landing a full second *after* its start hook.
//!
//! Why this is a separate process at all: the things that want this information
//! are usually sandboxed or short-lived — a terminal-multiplexer plugin, a
//! status bar, a notifier. The first consumer was a zellij plugin, which is
//! WASI-sandboxed and can never read ~/.claude or ~/.codex itself. Publishing
//! to a file that anything can read costs nothing and serves all of them.
//!
//! What this deliberately does NOT derive: "blocked". Neither agent records
//! permission prompts to disk — approval is UI state — so it arrives only by
//! report, through each agent's PermissionRequest hook, or from the screen when
//! nothing is observing.

mod agy;
mod claude;
mod codex;
mod completion;
mod cursor;
mod discover;
mod event;
mod event_log;
mod hook;
mod publish_lock;
mod query;
mod register;
mod tail;
mod ui;
mod wait;

use discover::Source;
use event::{Event, Kind, Snapshot};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
agentbus — observe coding agents, publish normalised events

USAGE:
    agentbus watch              Follow live transcripts and publish
    agentbus scan               Fold recent history once, print the snapshot
    agentbus events             Follow and print events to stdout only
    agentbus sessions           Query current state (see ASKING, below)
    agentbus ui                 Interactive system-wide agent roster
    agentbus wait               Block until a turn ends, and say what it said
    agentbus hook register      Publish this session's pane (from a hook)
    agentbus hook subagent start|stop
    agentbus hook state <state> [detail]

ASKING:
    agentbus sessions [--pid N] [--session S] [--cwd D] [--json]
    agentbus ui [--all]
    agentbus wait (--session S | --pid N) [--timeout SECS] [--since EPOCH[.FRAC]] [--json]

    --pid matches the named process or any descendant of it, so a supervisor
    can pass the pid it spawned without knowing that codex runs behind a shim.
    A codex session run by the shared app-server (codex 0.159+) is matched
    through the codex client under that pid, by its working directory.

    wait blocks until the current-or-next turn ends. It never reports a turn
    that ended before it started, so a caller that submits first should mark
    the moment — `t=$(date +%s.%N)` — and pass --since $t, or a turn that lands
    in the gap is missed. Keep the fraction: a whole-second mark still admits
    a previous turn that ended earlier in that second.

    wait exits 0 when the turn ended, 3 if the agent is blocked on a prompt,
    4 on timeout, 1 on resolution, observer, or retained-history errors.

OPTIONS:
    --within <MINS>   How recently a transcript must have changed (default 30)
    --interval <MS>   Poll interval (default 300)
    --snapshot <PATH> Where to write state   (default: state dir)
    --log <PATH>      Where to append events (default: state dir)
    --register <PATH> Session->pane registrations (default: state dir)
    --completions <PATH> Bounded turn completion index (default: state dir)
    --log-generation-mib <N> Rotate active log at MiB (default: 64)
    --log-max-mib <N> Total active and sealed log MiB (default: 512)
    --log-max-days <N> Maximum sealed history age (default: 7)
    --result-ttl <S>  How long a finished subagent's result stays published (90)
    --no-publish      Do not write snapshot or log
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprint!("{USAGE}");
        std::process::exit(2);
    }
    let mut opts = Opts::parse(&args);
    match args[0].as_str() {
        "hook" => hook::run(&args[1..], &opts.register, &opts.inbox, &opts.spool),
        "watch" => std::process::exit(run(&opts, true, true)),
        "events" => {
            // This verb is a reader that prints normalized live events. It must
            // remain usable beside the publisher and must never mutate shared
            // cursors, hook journals, or retained history.
            opts.publish = false;
            std::process::exit(run(&opts, true, false));
        }
        "scan" => std::process::exit(run(&opts, false, true)),
        // The reading verbs answer from what the observer published, so they
        // resolve its locations rather than assuming the defaults: the service
        // is routinely pointed elsewhere, and a reader guessing wrong reads a
        // stale file and reports nothing wrong.
        "sessions" => std::process::exit(query::run(&args[1..], &opts.resolved())),
        "ui" => std::process::exit(ui::run(&args[1..], &opts.resolved())),
        "wait" => std::process::exit(wait::run(&args[1..], &opts.resolved())),
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
    completions: PathBuf,
    event_log_limits: event_log::Limits,
    inbox: PathBuf,
    spool: PathBuf,
    /// Set when --snapshot was given. A pinned path may point into a directory
    /// created later by something else, so it is written only once that exists
    /// rather than being created here.
    snapshot_pinned: bool,
    result_ttl: Duration,
    publish: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Opts {
        let mut o = Opts {
            within: Duration::from_secs(30 * 60),
            interval: Duration::from_millis(300),
            snapshot: default_snapshot_path(),
            snapshot_pinned: false,
            log: state_dir().join("events.jsonl"),
            register: state_dir().join("register.jsonl"),
            completions: state_dir().join("completions.json"),
            event_log_limits: event_log::Limits::default(),
            inbox: hook::default_inbox(&state_dir()),
            spool: hook::default_spool(&state_dir()),
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
                        o.snapshot_pinned = true;
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
                "--completions" => {
                    if let Some(v) = next {
                        o.completions = PathBuf::from(v);
                    }
                }
                "--log-generation-mib" => {
                    if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                        o.event_log_limits.generation_bytes = v.saturating_mul(1024 * 1024).max(1);
                    }
                }
                "--log-max-mib" => {
                    if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                        o.event_log_limits.max_bytes = v.saturating_mul(1024 * 1024).max(1);
                    }
                }
                "--log-max-days" => {
                    if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                        o.event_log_limits.max_age_secs = v.saturating_mul(24 * 60 * 60);
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
        o.event_log_limits.generation_bytes = o
            .event_log_limits
            .generation_bytes
            .min(o.event_log_limits.max_bytes);
        o
    }

    /// What this invocation publishes, as a location record.
    fn locations(&self) -> query::Locations {
        query::Locations {
            snapshot: self.snapshot.clone(),
            log: self.log.clone(),
            register: self.register.clone(),
            completions: self.completions.clone(),
        }
    }

    /// Where to *read* from: what this invocation pinned, else wherever the
    /// running observer says it publishes.
    fn resolved(&self) -> query::Locations {
        query::resolve(&state_dir(), &self.locations(), self.snapshot_pinned)
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

/// Where state is published unless told otherwise.
///
/// Deliberately somewhere this process owns. A subscriber that can only read a
/// particular directory — a sandboxed plugin, say — should be given `--snapshot`
/// pointing there rather than having its location assumed here; knowledge of one
/// consumer's sandbox does not belong in the publisher.
fn default_snapshot_path() -> PathBuf {
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
    /// What Claude's normaliser carries between lines of this transcript.
    turn: claude::Turn,
}

fn run(opts: &Opts, follow: bool, print_snapshot: bool) -> i32 {
    let _publish_lock = if opts.publish {
        match publish_lock::PublishLock::acquire(&opts.log) {
            Ok(lock) => Some(lock),
            Err(error) => {
                eprintln!("agentbus: {error}");
                return 1;
            }
        }
    } else {
        None
    };
    // Say where this observer publishes, so readers need not hardcode it. Only
    // when actually publishing: a --no-publish run maintains none of these
    // files and must not point readers at them.
    if opts.publish {
        query::publish_locations(&state_dir(), &opts.locations());
    }
    let mut tails = tail::MultiTail::default();
    let mut streams: BTreeMap<PathBuf, Stream> = BTreeMap::new();
    let mut completions = completion::Index::load(&opts.completions);
    let cursor_path = state_dir().join("cursors.json");
    let mut saved_cursors = cursor::load(&cursor_path);
    // Seed files that predate the checkpoint feature at their current end. If
    // one later wakes up, it is primed through that prefix to rebuild parser
    // state without publishing historical bytes. The recency-limited active
    // set below is still replayed normally to construct the startup snapshot.
    for found in discover::active(Duration::MAX) {
        if saved_cursors.contains_key(&found.path) {
            continue;
        }
        let offset = std::fs::metadata(&found.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if let Some(saved) = cursor::SavedCursor::capture(
            &found.path,
            tail::TailCheckpoint {
                offset,
                partial: Vec::new(),
            },
        ) {
            saved_cursors.insert(found.path, saved);
        }
    }
    cursor::prune(&mut saved_cursors);
    let mut cursors_published = String::new();
    // The first pass replays history; transitions in it are dated from the
    // records themselves, never from the clock.
    let mut snap = Snapshot {
        backfilling: true,
        ..Default::default()
    };
    // Pane bindings reported through the inbox by agents with no transcript.
    let mut inbox_panes: BTreeMap<String, register::Pane> = BTreeMap::new();
    // Registrations from the previous pass, so the sidecar lookup knows each
    // session's transcript, plus bounded attempt counts per subagent.
    let mut regs_prev: BTreeMap<String, register::Pane> = BTreeMap::new();
    let mut meta_tries: BTreeMap<String, u32> = BTreeMap::new();
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
    let mut last_target = PathBuf::new();

    loop {
        let drained_hooks =
            opts.publish && hook::drain_to_journals(&opts.spool, &opts.register, &opts.inbox);
        let pruned_spool = if opts.publish {
            hook::prune_spool(&opts.spool, completion::now_epoch())
        } else {
            0
        };
        if pruned_spool > 0 {
            eprintln!("agentbus: pruned {pruned_spool} expired hook spool record(s)");
        }
        // Rediscovery each tick is what makes new sessions appear without
        // restarting; the recency filter keeps it to a handful of files.
        let found = discover::active(opts.within);
        let mut live: Vec<PathBuf> = Vec::new();
        for f in &found {
            live.push(f.path.clone());
            if tails.is_tracked(&f.path) {
                continue;
            }
            if !first {
                if let Some(saved) = saved_cursors
                    .get(&f.path)
                    .filter(|saved| saved.matches(&f.path))
                {
                    let mut stream = new_stream(f);
                    prime_stream(&mut stream, saved.tail.offset);
                    tails.restore(&f.path, saved.tail.clone());
                    streams.insert(f.path.clone(), stream);
                    continue;
                }
            }
            tails.track(&f.path, from_start);
            streams.insert(f.path.clone(), new_stream(f));
        }
        live.push(opts.inbox.clone());
        // Inactivity controls polling, not cursor ownership. Dropping a cursor
        // here made a transcript that became active again replay from byte zero
        // and republish its entire history. Keep the lightweight cursor and
        // normalizer state until the source file genuinely disappears.
        tails.drop_missing();
        streams.retain(|p, _| p.exists());

        // The inbox is tailed like a transcript: agents that cannot be read off
        // disk publish here, and so do reports no transcript carries.
        tails.track(&opts.inbox, true);
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
                    // log worth tailing is the point of an event bus.
                    if !remember_identity(st, &e) {
                        continue;
                    }
                    batch.push(e);
                }
            }
        }

        // Reports are folded after observations, not before.
        //
        // Within one pass the two are unordered — an inbox record carries no
        // timestamp — so whichever is applied last decides the state. A report
        // is the later fact by construction: hooks fire at the moment a thing
        // happens, while a transcript is read in bulk afterwards, sometimes
        // hours of it at startup.
        //
        // Applying them the other way round made a finished agy session read as
        // `working` forever after a restart: the backfill replayed its turn_end
        // from the inbox first, then replayed the prompt that opened that same
        // turn, and the prompt won. Live it looked right, which is the worst
        // way for this to be wrong.
        for line in tails.poll(&opts.inbox) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                batch.extend(inbox_events(&v, &mut inbox_panes));
            }
        }

        // Hooks no longer write these journals directly, so the observer can
        // compact them after consuming the current inbox cursor without racing
        // an already-open append descriptor.
        if opts.publish && (first || drained_hooks) {
            if hook::compact_inbox(&opts.inbox, completion::now_epoch()) {
                tails.reset_to_end(&opts.inbox);
            }
            let _ = register::compact(&opts.register, 10_000);
        }

        for e in &batch {
            snap.apply(e);
        }

        // Name subagents the hooks reported but could not name: Claude's
        // SubagentStart payload carries no agent_type or description, and the
        // subagent's own transcript may already have aged out of the window.
        // The sidecar beside the parent's transcript always has both.
        name_subagents(&mut snap, &regs_prev, &mut meta_tries);

        snap.expire(opts.result_ttl);

        // Registrations are re-read wholesale each pass, then verified against
        // the live process. A closed pane drops its mapping the same tick.
        let mut regs = register::load(&opts.register);
        regs_prev = regs.clone();
        // A registration knows things a report never does — the transcript
        // path above all — but on an unrecognised host it carries no location.
        // Where that is so, a report that does know one fills it in rather than
        // being turned away by an entry that has nothing to say about panes.
        for (k, v) in &inbox_panes {
            match regs.get_mut(k) {
                Some(existing) if existing.pane.is_empty() && !v.pane.is_empty() => {
                    existing.mux = v.mux.clone();
                    existing.mux_session = v.mux_session.clone();
                    existing.pane = v.pane.clone();
                }
                Some(_) => {}
                None => {
                    regs.insert(k.clone(), v.clone());
                }
            }
        }
        for (session, st) in snap.sessions.iter_mut() {
            match regs.get(session) {
                Some(p) if register::still_true(p) => {
                    st.mux = p.mux.clone();
                    st.mux_session = p.mux_session.clone();
                    st.pane = p.pane.clone();
                }
                _ => {
                    st.mux.clear();
                    st.mux_session.clear();
                    st.pane.clear();
                }
            }
        }
        // A `pane:` session is synthesised purely to give an agent with no
        // transcript somewhere to hang its pane binding. Two ways it stops
        // being meaningful:
        //
        //  - its binding died, leaving an entry describing nothing;
        //  - a real, transcript-backed session occupies the same pane, which
        //    makes the synthesised one a duplicate that shadows it. Subscribers
        //    index by pane, so a collision is resolved by whatever happens to
        //    sort last — and "pane:" sorts after most session ids, so the
        //    thinner entry silently won and the pane appeared stuck in whatever
        //    state it was last reported in.
        let claimed: std::collections::BTreeSet<String> = snap
            .sessions
            .iter()
            .filter(|(id, st)| !id.starts_with("pane:") && !st.pane.is_empty())
            .map(|(_, st)| format!("{}/{}/{}", st.mux, st.mux_session, st.pane))
            .collect();
        snap.sessions.retain(|id, st| {
            if !id.starts_with("pane:") {
                return true;
            }
            !st.pane.is_empty()
                && !claimed.contains(&format!("{}/{}/{}", st.mux, st.mux_session, st.pane))
        });

        // `first` is the backfill pass: real history, but not news.
        if opts.publish && !batch.is_empty() && !first {
            if completions.push_events(&batch, completion::now_epoch())
                && !completions.write_atomic(&opts.completions)
            {
                eprintln!(
                    "agentbus: cannot publish completion index {}",
                    opts.completions.display()
                );
            }
            if let Err(error) = event_log::append(
                &opts.log,
                &batch,
                opts.event_log_limits,
                completion::now_epoch(),
            ) {
                eprintln!("agentbus: {error}");
            }
        }
        // Publish on any observable difference, not on "did events arrive".
        // Expiry and a registration going stale both change what is true while
        // producing no event, so keying the write off the batch left the file
        // saying things that had stopped being so.
        if opts.publish {
            let txt = serde_json::to_string(&snap.to_json()).unwrap_or_default();
            if (txt != published || last_target.as_os_str().is_empty())
                && write_snapshot(&opts.snapshot, &txt, !opts.snapshot_pinned)
            {
                published = txt;
                last_target = opts.snapshot.clone();
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
        snap.backfilling = false;

        // A --no-publish observer must not advance the live publishers
        // publication cursor: doing so would make the next restart skip events
        // this process printed but never put on the bus.
        if opts.publish {
            for (path, checkpoint) in tails.checkpoints() {
                if !streams.contains_key(&path) {
                    continue;
                }
                if let Some(saved) = cursor::SavedCursor::capture(&path, checkpoint) {
                    saved_cursors.insert(path, saved);
                }
            }
            cursor::prune(&mut saved_cursors);
            let cursor_text = cursor::encode(&saved_cursors);
            if cursor_text != cursors_published && cursor::write_atomic(&cursor_path, &cursor_text)
            {
                cursors_published = cursor_text;
            }
        }

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
    0
}

/// Rebuild source-specific parser state through a previously published prefix.
/// Events produced here are intentionally discarded: the saved cursor proves
/// that prefix is history, while parsing it is necessary for facts such as the
/// last Claude assistant answer carried into a later turn-end record.
fn prime_stream(stream: &mut Stream, offset: u64) {
    let Ok(file) = std::fs::File::open(&stream.path) else {
        return;
    };
    let mut reader = BufReader::new(file.take(offset));
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // Bytes, not read_line: one line with invalid UTF-8 must not end
        // priming. Decoding is lossy per complete line, like the live tail.
        let Ok(read) = reader.read_until(b'\n', &mut buf) else {
            return;
        };
        if read == 0 {
            return;
        }
        // A saved partial line is held by MultiTail and completed by the next
        // append. Feeding it to a JSON parser here would either do nothing or
        // reconstruct the wrong source state.
        if buf.last() != Some(&b'\n') {
            return;
        }
        let line = String::from_utf8_lossy(&buf);
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else {
            continue;
        };
        for event in events_for(stream, &value) {
            remember_identity(stream, &event);
        }
    }
}

fn remember_identity(stream: &mut Stream, event: &Event) -> bool {
    let Kind::Session {
        cwd, model, title, ..
    } = &event.kind
    else {
        return true;
    };
    let fingerprint = format!(
        "{}|{}|{}",
        cwd.clone().unwrap_or_default(),
        model.clone().unwrap_or_default(),
        title.clone().unwrap_or_default()
    );
    if stream.last_identity.as_deref() == Some(fingerprint.as_str()) {
        return false;
    }
    stream.last_identity = Some(fingerprint);
    true
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
        fallback: match f.source {
            // Every agy transcript is named `transcript.jsonl`; the id is the
            // directory it sits under, so a filename-derived one would make
            // every conversation the same session.
            Source::Agy => agy::session_of(&f.path),
            // Codex filenames are rollout-<ts>-<uuid>; the uuid is the tail.
            _ => stem
                .rsplit_once('-')
                .map(|(_, id)| id.to_string())
                .unwrap_or(stem),
        },
        parent: f.parent_session.clone(),
        agent_id,
        agent_type,
        description,
        last_identity: None,
        turn: claude::Turn::default(),
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
                claude::normalize_subagent(v, parent, &st.agent_id, &st.agent_type, &st.description)
            }
            None => claude::normalize(v, &st.fallback, &mut st.turn),
        },
        Source::Codex => {
            // A subagent's rollout must be recognised before anything else reads
            // session_id off it: that field holds the *parent's* id, so treating
            // the file as an ordinary session files the subagent's prompts under
            // the parent and overwrites the parent's label.
            if let Some((parent, own, nick)) = codex::subagent_of(v) {
                st.parent = Some(parent);
                st.agent_id = own;
                st.description = nick;
            } else if st.parent.is_none() {
                // Only session_meta carries the session id; every later line
                // would otherwise fall back to a filename-derived one and show
                // up as a second, phantom session. Learn it once, then reuse it.
                if let Some(id) = v.pointer("/payload/session_id").and_then(|x| x.as_str()) {
                    st.fallback = id.to_string();
                }
            }
            match &st.parent {
                Some(parent) => codex::normalize_subagent(v, parent, &st.agent_id, &st.description),
                None => codex::normalize(v, &st.fallback),
            }
        }
        // agy names its session nowhere in the transcript, so the id derived
        // from the path is not a fallback here but the only source.
        Source::Agy => agy::normalize(v, &st.fallback),
    }
}

/// Written via a temp file and renamed, so a subscriber polling the path never
/// reads a half-written snapshot. The plugin polls this once a second.
/// Returns whether the snapshot was written.
///
/// `create_dir` is false for a path the caller pinned: that may live in a
/// directory another program owns and has not created yet — a multiplexer's
/// runtime dir, for instance — and creating it here would take ownership of
/// something with its own expectations about permissions. Waiting costs a few
/// ticks and cannot break anything.
fn write_snapshot(path: &std::path::Path, txt: &str, create_dir: bool) -> bool {
    match path.parent() {
        Some(dir) if create_dir => {
            let _ = std::fs::create_dir_all(dir);
        }
        Some(dir) if !dir.is_dir() => return false,
        _ => {}
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, txt).is_ok() {
        return std::fs::rename(&tmp, path).is_ok();
    }
    false
}

/// Translate an inbox record into normalised events.
///
/// These come from agents rather than from disk, so they are reports rather
/// than observations — but they carry exactly what the transcripts cannot: a
/// subagent's completion and result, and the state of an agent that writes no
/// transcript we can read.
fn inbox_events(v: &serde_json::Value, panes: &mut BTreeMap<String, register::Pane>) -> Vec<Event> {
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
    let pane = g("pane");
    if !pane.is_empty() {
        panes.insert(
            session.clone(),
            register::Pane {
                mux: g("mux"),
                mux_session: g("mux_session"),
                pane,
                // Reports carry no transcript; only a registration knows it.
                transcript: String::new(),
                pid: v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0),
                starttime: v.get("starttime").and_then(|x| x.as_u64()).unwrap_or(0),
                ..Default::default()
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
                model: None,
                effort: None,
                tool: None,
            })
        }
        "state" => {
            let st = g("state");
            let detail = g("detail");
            mk(Kind::Reported { state: st, detail })
        }
        // Identity for a host whose transcript carries none, reported at the
        // start of a turn so the session is named while it works rather than
        // only once it has finished.
        "session" => identity_event(&session, &g("cwd"), &g("model")),
        // The end of a turn, for a host that records it nowhere. The report
        // carries the boundary and the transcript that holds the answer, never
        // the answer itself: an inbox line is dropped whole above the atomic
        // append limit, which would lose exactly the long answers worth having,
        // and lose them without a sound. Reading the file here has neither
        // problem, and the observer is already reading it anyway.
        "turn_end" => {
            let (result, result_full) = match g("transcript") {
                t if t.is_empty() => (None, None),
                t => agy::last_answer(&agy::transcript_path(&t)),
            };
            // Identity rides along because it is the only place agy publishes
            // it: its transcript names neither the working directory nor the
            // model, and both arrive on every hook payload for free.
            let mut out = identity_event(&session, &g("cwd"), &g("model"));
            out.push(Event {
                ts: String::new(),
                source: "hook",
                session,
                kind: Kind::TurnEnd {
                    duration_ms: v.get("duration_ms").and_then(|x| x.as_u64()),
                    result,
                    result_full,
                },
            });
            out
        }
        _ => Vec::new(),
    }
}

/// A reported `cwd`/`model`, as an event — or nothing, when neither was sent.
///
/// Empty rather than a single `Kind::Session`, because publishing one with both
/// fields `None` would restate identity on every turn and fill the log with
/// events that say nothing.
fn identity_event(session: &str, cwd: &str, model: &str) -> Vec<Event> {
    if cwd.is_empty() && model.is_empty() {
        return Vec::new();
    }
    vec![Event {
        ts: String::new(),
        source: "hook",
        session: session.to_string(),
        kind: Kind::Session {
            title: None,
            title_rank: 0,
            cwd: (!cwd.is_empty()).then(|| cwd.to_string()),
            model: (!model.is_empty()).then(|| model.to_string()),
            effort: None,
        },
    }]
}

/// How many passes to keep looking for a subagent's sidecar before giving up.
/// Claude writes it about a second after the start hook fires, so it must be
/// retried — but a subagent that never gets one must not be stat'd forever.
const META_TRIES: u32 = 60;

/// Fill in names for subagents that were reported but not described.
///
/// Claude's SubagentStart payload carries neither `agent_type` nor
/// `description`; the old shell hook forked a background poller to chase the
/// sidecar, which is exactly the kind of work an observer should be doing
/// instead. The subagent's own transcript also carries it, but only while that
/// file is still inside the recency window — the sidecar is reachable from the
/// parent's transcript path regardless of age.
fn name_subagents(
    snap: &mut Snapshot,
    regs: &BTreeMap<String, register::Pane>,
    tries: &mut BTreeMap<String, u32>,
) {
    for (session, st) in snap.sessions.iter_mut() {
        let Some(pane) = regs.get(session) else {
            continue;
        };
        // Identified by where the transcript lives rather than by `source`:
        // a session seen only through hooks has no agent of its own recorded,
        // and those are exactly the ones still carrying unfiltered noise.
        // Codex names its subagents from its own rollout, so looking for this
        // layout anywhere else just burns syscalls.
        if !pane.transcript.contains("/.claude/") {
            continue;
        }
        let mut drop_ids: Vec<String> = Vec::new();
        for (id, sub) in st.subagents.iter_mut() {
            if !sub.agent_type.is_empty() && !sub.description.is_empty() {
                continue;
            }
            let path = claude::subagent_meta_path(&pane.transcript, id);
            let n = tries.entry(format!("{session}/{id}")).or_insert(0);
            if *n >= META_TRIES {
                // Never acquired a sidecar, so it is not a Task subagent at
                // all. Claude also fires the subagent hooks for its own
                // internal agents — the ones that write suggested prompts and
                // conversation recaps — whose "result" is UI text rather than
                // work anyone asked for. They have no sidecar, ever, which is
                // what tells them apart.
                if sub.agent_type.is_empty() && !path.with_extension("meta.json").exists() {
                    drop_ids.push(id.clone());
                }
                continue;
            }
            *n += 1;
            let (t, d) = claude::subagent_meta(&path);
            if !t.is_empty() {
                sub.agent_type = t;
            }
            if !d.is_empty() {
                sub.description = d;
            }
        }
        for id in drop_ids {
            st.subagents.remove(&id);
        }
    }
}

#[cfg(test)]
mod stream_checkpoint_tests {
    use super::*;

    #[test]
    fn priming_restores_the_claude_answer_without_republishing_it() {
        let dir = std::env::temp_dir().join(format!("agentbus-prime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let assistant = serde_json::json!({
            "type": "assistant",
            "sessionId": "session",
            "timestamp": "2026-08-09T12:00:00Z",
            "message": { "content": [{ "type": "text", "text": "the full answer" }] }
        });
        std::fs::write(&path, format!("{assistant}\n")).unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();
        let found = discover::Found {
            path: path.clone(),
            source: Source::Claude,
            parent_session: None,
        };
        let mut stream = new_stream(&found);
        prime_stream(&mut stream, offset);

        let end = serde_json::json!({
            "type": "system",
            "subtype": "turn_duration",
            "sessionId": "session",
            "timestamp": "2026-08-09T12:00:01Z",
            "durationMs": 1000
        });
        let events = events_for(&mut stream, &end);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            Kind::TurnEnd { result_full, .. } => {
                assert_eq!(result_full.as_deref(), Some("the full answer"));
            }
            other => panic!("expected turn_end, got {other:?}"),
        }
    }

    #[test]
    fn priming_continues_past_a_line_with_invalid_utf8() {
        let dir = std::env::temp_dir().join(format!("agentbus-prime-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let assistant = serde_json::json!({
            "type": "assistant",
            "sessionId": "session",
            "timestamp": "2026-08-09T12:00:00Z",
            "message": { "content": [{ "type": "text", "text": "answer after bad bytes" }] }
        });
        let mut bytes = b"{\"type\":\"user\",\"text\":\"broken \xff\xfe byte\"}\n".to_vec();
        bytes.extend_from_slice(format!("{assistant}\n").as_bytes());
        // A trailing partial line must still be left alone.
        bytes.extend_from_slice(b"{\"type\":\"assistant\",\"message\":");
        std::fs::write(&path, &bytes).unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();
        let found = discover::Found {
            path: path.clone(),
            source: Source::Claude,
            parent_session: None,
        };
        let mut stream = new_stream(&found);
        prime_stream(&mut stream, offset);

        let end = serde_json::json!({
            "type": "system",
            "subtype": "turn_duration",
            "sessionId": "session",
            "timestamp": "2026-08-09T12:00:01Z",
            "durationMs": 1000
        });
        let events = events_for(&mut stream, &end);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            Kind::TurnEnd { result_full, .. } => {
                assert_eq!(result_full.as_deref(), Some("answer after bad bytes"));
            }
            other => panic!("expected turn_end, got {other:?}"),
        }
    }
}
