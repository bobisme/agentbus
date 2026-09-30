//! `agentbus wait` — block until a turn ends, and say what it said.
//!
//! The verb a supervisor actually wants. Without it, "tell me when this agent
//! is done" means locating the snapshot, polling it, noticing a working->idle
//! transition, avoiding the *previous* turn's answer, and then going to find
//! the answer itself — 145 lines in the first supervisor written against this,
//! 18% of the script, none of it domain logic.
//!
//! All of it is bookkeeping this process is better placed to do, being already
//! the owner of the completion projection. One of the steps is not merely tedious but
//! genuinely impossible for the caller to get right: see `WATERMARK` below.

use crate::completion::{Index, Lookup};
use crate::query::{self, Filter, Locations, Resolved};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Exit codes. A supervisor branches on these, so they are part of the
/// interface: `blocked` and `timeout` look identical to any screen-based waiter
/// and must not look identical here.
const OK: i32 = 0;
const ERR: i32 = 1;
const USAGE: i32 = 2;
const BLOCKED: i32 = 3;
const TIMEOUT: i32 = 4;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900);
const POLL: Duration = Duration::from_millis(150);

/// Why the watermark is the whole difficulty.
///
/// A caller that submits a prompt and then asks "is it idle yet" can win that
/// race: between submission and the agent picking the work up, the session is
/// still idle *from the previous turn*. The caller sees idle, takes the answer
/// sitting there, and returns the previous turn's response as this turn's.
/// Measured, a trivial codex turn completes in 1.1s, so even a 1s poll can
/// straddle an entire turn and miss it in both directions.
///
/// Here it is a monotone generation from the bounded completion index, taken
/// before anything else happens. Only turn_ends published after it satisfy the wait. That
/// also makes "current-or-next" fall out for free rather than needing a rule: a
/// turn already running ends after the generation, and so does a turn that has not
/// started, while the one that ended before the call is behind it and
/// unreachable.
///
/// A caller cannot do this for itself across two processes without persisting
/// the watermark to a file — which is exactly what the supervisor this replaces
/// had to do.
const _WATERMARK: () = ();

/// The turn a caller means, when it submitted before calling this.
///
/// The watermark makes a turn that ended before entry unreachable, which is
/// right for the previous turn and wrong for the one the caller just started:
/// submit, then call, and a turn that finished in the gap is missed, leaving
/// the caller waiting for a turn that will never come. The two requirements —
/// never the previous answer, always the current one — are only separable if
/// something names the moment of submission, and the caller is the only thing
/// that knows it.
///
/// So `--since <epoch>` is the caller's own reference point: shell `date +%s`
/// before submitting, pass it here. It costs no file and no bookkeeping, which
/// is what a caller had to do instead. Without it the behaviour is unchanged.
///
/// Fractional seconds are accepted, and needed by a caller that drives one
/// session turn after turn: it marks the next turn straight after the last
/// wait returned, often in the second that turn ended, and a whole-second mark
/// cannot exclude it (bn-m77). `date +%s.%N` can.
struct Args {
    filter: Filter,
    timeout: Duration,
    json: bool,
    /// Epoch milliseconds.
    since: Option<u64>,
}

fn parse(args: &[String]) -> Args {
    let mut a = Args {
        filter: Filter::default(),
        timeout: DEFAULT_TIMEOUT,
        json: false,
        since: None,
    };
    let mut i = 0;
    while i < args.len() {
        let next = args.get(i + 1);
        match args[i].as_str() {
            "--session" => a.filter.session = next.cloned(),
            "--pid" => a.filter.pid = next.and_then(|v| v.parse().ok()),
            "--cwd" => a.filter.cwd = next.cloned(),
            "--timeout" => {
                if let Some(v) = next.and_then(|v| v.parse::<u64>().ok()) {
                    a.timeout = Duration::from_secs(v);
                }
            }
            "--since" => a.since = next.and_then(|v| since_ms(v)),
            "--json" => a.json = true,
            _ => {}
        }
        i += 1;
    }
    a
}

/// Epoch seconds, whole or fractional, as epoch milliseconds. Digits past the
/// millisecond are dropped, which moves the mark earlier and so can admit a
/// turn that ended in that same millisecond but never miss one.
fn since_ms(v: &str) -> Option<u64> {
    let (whole, fraction) = v.split_once('.').unwrap_or((v, ""));
    let digits = |t: &str| t.bytes().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    let mut ms = whole.parse::<u64>().ok()?.checked_mul(1000)?;
    let mut unit = 100;
    for c in fraction.bytes().take(3) {
        ms += u64::from(c - b'0') * unit;
        unit /= 10;
    }
    Some(ms)
}

/// Pull the answer off a turn_end record, full text first.
///
/// Falls back to the preview so this still says something useful against an
/// observer old enough not to publish one, and against a source that only ever
/// had the preview.
fn result_of(v: &Value) -> Option<String> {
    v.get("result_full")
        .or_else(|| v.get("result"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// What the wait ended as. `result` is the full answer where there is one.
struct Outcome {
    status: &'static str,
    session: String,
    result: Option<String>,
    duration_ms: Option<u64>,
    detail: String,
}

impl Outcome {
    fn code(&self) -> i32 {
        match self.status {
            "done" => OK,
            "blocked" => BLOCKED,
            "timeout" => TIMEOUT,
            "expired" => ERR,
            _ => ERR,
        }
    }

    /// The object `--json` prints. Optional keys are omitted, not null.
    fn to_json(&self) -> Value {
        let mut v = json!({
            "status": self.status,
            "session": self.session,
        });
        let o = v.as_object_mut().unwrap();
        if let Some(r) = &self.result {
            o.insert("result".into(), json!(r));
        }
        if let Some(d) = self.duration_ms {
            o.insert("duration_ms".into(), json!(d));
        }
        if !self.detail.is_empty() {
            o.insert("detail".into(), json!(self.detail));
        }
        v
    }

    fn print(&self, as_json: bool) {
        self.emit(
            as_json,
            &mut std::io::stdout().lock(),
            &mut std::io::stderr(),
        );
    }

    /// Write the outcome: the answer or JSON object to `out`, anything a
    /// caller should not capture as the answer to `err`.
    fn emit(&self, as_json: bool, out: &mut impl std::io::Write, err: &mut impl std::io::Write) {
        if as_json {
            query::line(
                out,
                &serde_json::to_string(&self.to_json()).unwrap_or_default(),
            );
            return;
        }
        match self.status {
            // The answer alone on stdout, so `r=$(agentbus wait …)` is the
            // whole integration for a caller that only wants the text.
            "done" => {
                if let Some(r) = &self.result {
                    query::line(out, r);
                }
            }
            _ => {
                let detail = if self.detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", self.detail)
                };
                query::line(err, &format!("agentbus: {}{detail}", self.status));
            }
        }
    }
}

/// The state a session is in right now, straight from the snapshot.
fn state_of(loc: &Locations, session: &str) -> Option<(String, String)> {
    let s = query::by_id(loc).remove(session)?;
    let g = |k: &str| {
        s.state
            .get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    Some((g("state"), g("detail")))
}

/// The `blocked` outcome, if the snapshot says the session is waiting on a
/// prompt. The detail is the snapshot's own.
fn blocked_outcome(loc: &Locations, session: &str) -> Option<Outcome> {
    let (st, detail) = state_of(loc, session)?;
    (st == "blocked").then(|| Outcome {
        status: "blocked",
        session: session.to_string(),
        result: None,
        duration_ms: None,
        detail,
    })
}

pub fn run(args: &[String], loc: &Locations) -> i32 {
    let a = parse(args);
    if a.filter.is_empty() {
        eprintln!("agentbus: wait needs --session, --pid or --cwd");
        return USAGE;
    }

    // Order matters. The watermark is taken before the session is resolved, so
    // a turn that ends during resolution is still caught: resolving reads the
    // snapshot and the register off disk, which is not instant, and a fast turn
    // fits inside it.
    let entry_generation = Index::load(&loc.completions).generation;

    let deadline = Instant::now() + a.timeout;

    // Wait for the session to exist rather than failing on the spot.
    //
    // A supervisor submits and then waits, but an agent does not appear here
    // until it registers, which for codex is its first prompt — so at the
    // moment the caller calls, the session it just gave work to routinely does
    // not exist yet. Failing then would push a retry loop back onto every
    // caller, which is the bookkeeping this verb exists to absorb.
    //
    // Deliberately not polling the log during this: the cursor is already at
    // the entry watermark and only advances when read, so a turn_end arriving
    // while the session is still nameless is still there once it has a name.
    let session = loop {
        match query::resolve_one(loc, &a.filter) {
            // No snapshot at all is the observer being down, which waiting
            // cannot fix. Distinct from "not registered yet", which it does.
            None => {
                eprintln!(
                    "agentbus: no snapshot at {}; is the observer running?",
                    loc.snapshot.display()
                );
                return ERR;
            }
            Some(Resolved::One(s)) => break s.id.clone(),
            // Ambiguity is the caller's to resolve and will not clear itself,
            // and neither will a pid that cannot name a session.
            Some(r @ (Resolved::Many(_) | Resolved::Unbindable(_))) => {
                eprintln!("agentbus: {}", query::describe_miss(&a.filter, &r));
                return ERR;
            }
            Some(Resolved::None) => {
                if Instant::now() >= deadline {
                    return finish(
                        Outcome {
                            status: "timeout",
                            session: String::new(),
                            result: None,
                            duration_ms: None,
                            detail: format!(
                                "{} within {}s",
                                query::describe_miss(&a.filter, &Resolved::None),
                                a.timeout.as_secs()
                            ),
                        },
                        a.json,
                    );
                }
                std::thread::sleep(POLL);
            }
        }
    };

    // A turn the caller started that finished before this process got going.
    // Checked before `blocked`, because a finished turn is a finished turn: an
    // agent that answered and then hit a permission prompt on the *next* thing
    // has still answered the question being waited on.
    if let Some(since) = a.since {
        match Index::load(&loc.completions).newest_since(&session, since) {
            Lookup::Found(record) => return finish(done(&session, &record), a.json),
            Lookup::Expired { floor } => {
                return finish(
                    Outcome {
                        status: "expired",
                        session,
                        result: None,
                        duration_ms: None,
                        detail: format!("completion history before generation {floor} has expired"),
                    },
                    a.json,
                );
            }
            Lookup::Pending => {}
        }
    }

    // A session already sitting on a permission prompt is answered at once.
    // Waiting on one is how a supervisor burns its entire timeout: to anything
    // watching a screen it looks exactly like an agent thinking hard.
    if let Some(o) = blocked_outcome(loc, &session) {
        return finish(o, a.json);
    }

    loop {
        match Index::load(&loc.completions).first_after(&session, entry_generation) {
            Lookup::Found(record) => return finish(done(&session, &record), a.json),
            Lookup::Expired { floor } => {
                return finish(
                    Outcome {
                        status: "expired",
                        session,
                        result: None,
                        duration_ms: None,
                        detail: format!("completion watermark expired before generation {floor}"),
                    },
                    a.json,
                );
            }
            Lookup::Pending => {}
        }

        // Checked every pass rather than only at entry: a permission prompt
        // usually arrives mid-turn, which is precisely the case a caller is
        // blocked on when it happens.
        if let Some(o) = blocked_outcome(loc, &session) {
            return finish(o, a.json);
        }

        if Instant::now() >= deadline {
            return finish(
                Outcome {
                    status: "timeout",
                    session,
                    result: None,
                    duration_ms: None,
                    detail: format!("no turn ended within {}s", a.timeout.as_secs()),
                },
                a.json,
            );
        }
        std::thread::sleep(POLL);
    }
}

fn done(session: &str, record: &Value) -> Outcome {
    Outcome {
        status: "done",
        session: session.to_string(),
        result: result_of(record),
        duration_ms: record.get("duration_ms").and_then(Value::as_u64),
        detail: String::new(),
    }
}

fn finish(o: Outcome, as_json: bool) -> i32 {
    o.print(as_json);
    o.code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Kind};

    fn fixture(name: &str) -> Locations {
        let dir = std::env::temp_dir().join(format!("agentbus-wait-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let snapshot = dir.join("snapshot.json");
        std::fs::write(
            &snapshot,
            r#"{"sessions":[{"session":"s","state":"idle"}]}"#,
        )
        .unwrap();
        Locations {
            snapshot,
            log: dir.join("events.jsonl"),
            register: dir.join("register.jsonl"),
            completions: dir.join("completions.json"),
        }
    }

    fn write_completion(loc: &Locations) {
        let event = Event {
            ts: "2026-08-09T12:00:00Z".into(),
            source: "test",
            session: "s".into(),
            kind: Kind::TurnEnd {
                duration_ms: Some(7),
                result: Some("answer".into()),
                result_full: Some("full answer".into()),
            },
        };
        let mut index = Index::default();
        index.push_events(
            &[event],
            crate::event::iso_to_epoch("2026-08-09T12:00:01Z").unwrap(),
        );
        assert!(index.write_atomic(&loc.completions));
    }

    #[test]
    fn entry_generation_rejects_a_stale_answer() {
        let loc = fixture("stale");
        write_completion(&loc);
        assert_eq!(
            run(
                &[
                    "--session".into(),
                    "s".into(),
                    "--timeout".into(),
                    "0".into()
                ],
                &loc,
            ),
            TIMEOUT
        );
    }

    #[test]
    fn since_finds_a_retained_completion_without_the_verbose_log() {
        let loc = fixture("since");
        write_completion(&loc);
        assert!(!loc.log.exists());
        let since = crate::event::iso_to_epoch("2026-08-09T11:59:59Z")
            .unwrap()
            .to_string();
        assert_eq!(
            run(
                &[
                    "--session".into(),
                    "s".into(),
                    "--since".into(),
                    since,
                    "--json".into(),
                ],
                &loc,
            ),
            OK
        );
    }

    #[test]
    fn completion_during_session_resolution_is_not_missed() {
        let loc = fixture("resolution-race");
        std::fs::write(&loc.snapshot, r#"{"sessions":[]}"#).unwrap();
        let writer_loc = loc.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            write_completion(&writer_loc);
            std::fs::write(
                &writer_loc.snapshot,
                r#"{"sessions":[{"session":"s","state":"idle"}]}"#,
            )
            .unwrap();
        });
        let code = run(
            &[
                "--session".into(),
                "s".into(),
                "--timeout".into(),
                "2".into(),
            ],
            &loc,
        );
        writer.join().unwrap();
        assert_eq!(code, OK);
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn write_snapshot(loc: &Locations, sessions: &str) {
        std::fs::write(&loc.snapshot, format!(r#"{{"sessions":{sessions}}}"#)).unwrap();
    }

    fn stat_starttime(pid: u64) -> u64 {
        let txt = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        crate::register::parse_stat(&txt).unwrap().1
    }

    /// `bin 60` running in `cwd`, killed on drop. Through a symlink named
    /// `codex` it is a codex client as far as `/proc` can tell.
    struct Sleeper(std::process::Child);

    impl Sleeper {
        fn spawn(bin: &std::path::Path, cwd: &std::path::Path) -> Self {
            let child = std::process::Command::new(bin)
                .arg("60")
                .current_dir(cwd)
                .spawn()
                .unwrap();
            Self(child)
        }

        fn pid(&self) -> u64 {
            u64::from(self.0.id())
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// bn-3c9. Codex 0.159 runs every client's hooks in one shared
    /// app-server, so each session registers that host rather than its own
    /// client. Two clients in their own directories under one live host (a
    /// process of its own, as the daemon is), plus an older session in the first
    /// client's directory from before that client started: `--pid` of each
    /// client finds its own session, and the host's pid is refused at once.
    #[test]
    fn pid_binds_each_client_of_a_shared_host_to_its_own_session() {
        let loc = fixture("shared-host");
        let root = loc.snapshot.parent().unwrap().to_path_buf();
        let (dir_a, dir_b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let codex = root.join("codex");
        std::os::unix::fs::symlink("/bin/sleep", &codex).unwrap();
        let (a, b) = (
            Sleeper::spawn(&codex, &dir_a),
            Sleeper::spawn(&codex, &dir_b),
        );
        // Let both exec before /proc is read for their command lines.
        let deadline = Instant::now() + Duration::from_secs(5);
        while [a.pid(), b.pid()].iter().any(|pid| {
            !std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| c.starts_with(codex.as_os_str().as_encoded_bytes()))
        }) {
            assert!(Instant::now() < deadline, "fake clients never exec'd");
            std::thread::sleep(Duration::from_millis(10));
        }

        let daemon = Sleeper::spawn(std::path::Path::new("/bin/sleep"), &root);
        let host = daemon.pid();
        let host_start = stat_starttime(host);
        let register = |session: &str, tick: u64| {
            json!({
                "session_id": session,
                "pid": 0,
                "starttime": 0,
                "host_pid": host,
                "host_starttime": host_start,
                "registered_tick": tick,
            })
            .to_string()
        };
        let (start_a, start_b) = (stat_starttime(a.pid()), stat_starttime(b.pid()));
        std::fs::write(
            &loc.register,
            [
                register("old-a", start_a - 1),
                register("sa", start_a),
                register("sb", start_b + 1),
            ]
            .join("\n"),
        )
        .unwrap();
        let (a_dir, b_dir) = (dir_a.display(), dir_b.display());
        write_snapshot(
            &loc,
            &format!(
                r#"[{{"session":"old-a","state":"idle","cwd":"{a_dir}"}},
                    {{"session":"sa","state":"working","cwd":"{a_dir}"}},
                    {{"session":"sb","state":"working","cwd":"{b_dir}"}}]"#
            ),
        );

        let resolve = |pid: u64| {
            let filter = Filter {
                pid: Some(pid),
                ..Filter::default()
            };
            match query::resolve_one(&loc, &filter) {
                Some(Resolved::One(s)) => {
                    assert_eq!(s.presence(), query::Presence::Hosted);
                    s.id.clone()
                }
                Some(r) => format!("miss: {}", query::describe_miss(&filter, &r)),
                None => "no snapshot".into(),
            }
        };
        assert_eq!(resolve(a.pid()), "sa");
        assert_eq!(resolve(b.pid()), "sb");
        assert!(
            resolve(host).contains("shared app-server"),
            "{}",
            resolve(host)
        );

        // And `wait` on each gets its own session's answer, or none.
        let mut index = Index::default();
        index.push_events(
            &[Event {
                ts: "2026-08-09T12:00:00Z".into(),
                source: "test",
                session: "sa".into(),
                kind: Kind::TurnEnd {
                    duration_ms: Some(7),
                    result: Some("a's answer".into()),
                    result_full: Some("a's answer".into()),
                },
            }],
            crate::event::iso_to_epoch("2026-08-09T12:00:01Z").unwrap(),
        );
        assert!(index.write_atomic(&loc.completions));
        let wait = |pid: u64| {
            run(
                &args(&["--pid", &pid.to_string(), "--since", "0", "--timeout", "0"]),
                &loc,
            )
        };
        assert_eq!(wait(a.pid()), OK);
        assert_eq!(wait(b.pid()), TIMEOUT);
        let started = Instant::now();
        assert_eq!(wait(host), ERR);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    fn outcome(status: &'static str) -> Outcome {
        Outcome {
            status,
            session: "s".into(),
            result: None,
            duration_ms: None,
            detail: String::new(),
        }
    }

    #[test]
    fn a_blocked_session_returns_three_at_once_with_the_snapshot_detail() {
        let loc = fixture("blocked");
        write_snapshot(
            &loc,
            r#"[{"session":"s","state":"blocked","detail":"needs approval"}]"#,
        );
        let started = Instant::now();
        let a = parse(&args(&["--session", "s", "--timeout", "30"]));
        assert_eq!(a.timeout, Duration::from_secs(30));
        let code = run(&args(&["--session", "s", "--timeout", "30"]), &loc);
        assert_eq!(code, BLOCKED);
        assert!(started.elapsed() < Duration::from_secs(5));

        // The detail is the snapshot's, carried through to the outcome.
        let o = blocked_outcome(&loc, "s").unwrap();
        assert_eq!((o.status, o.session.as_str()), ("blocked", "s"));
        assert_eq!(o.detail, "needs approval");
        assert!(o.result.is_none() && o.duration_ms.is_none());
        assert!(blocked_outcome(&loc, "nope").is_none());
        write_snapshot(&loc, r#"[{"session":"s","state":"idle"}]"#);
        assert!(blocked_outcome(&loc, "s").is_none());
    }

    #[test]
    fn state_of_is_none_for_an_unknown_session() {
        let loc = fixture("state-of-none");
        assert_eq!(state_of(&loc, "nope"), None);
        assert_eq!(
            state_of(&loc, "s"),
            Some(("idle".to_string(), String::new()))
        );
    }

    #[test]
    fn a_session_that_blocks_mid_wait_is_reported_blocked() {
        let loc = fixture("blocks-later");
        let writer_loc = loc.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            write_snapshot(
                &writer_loc,
                r#"[{"session":"s","state":"blocked","detail":"prompt"}]"#,
            );
        });
        let started = Instant::now();
        let code = run(&args(&["--session", "s", "--timeout", "30"]), &loc);
        writer.join().unwrap();
        assert_eq!(code, BLOCKED);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_finished_turn_beats_a_blocked_state_when_since_names_it() {
        let loc = fixture("since-before-blocked");
        write_completion(&loc);
        write_snapshot(&loc, r#"[{"session":"s","state":"blocked"}]"#);
        let since = crate::event::iso_to_epoch("2026-08-09T11:59:59Z")
            .unwrap()
            .to_string();
        assert_eq!(run(&args(&["--session", "s", "--since", &since]), &loc), OK);
    }

    #[test]
    fn json_output_carries_every_populated_key() {
        let full = Outcome {
            status: "done",
            session: "s".into(),
            result: Some("the answer".into()),
            duration_ms: Some(42),
            detail: "why".into(),
        };
        assert_eq!(
            full.to_json(),
            json!({
                "status": "done",
                "session": "s",
                "result": "the answer",
                "duration_ms": 42,
                "detail": "why",
            })
        );
    }

    #[test]
    fn json_output_omits_absent_optional_keys() {
        let v = outcome("timeout").to_json();
        assert_eq!(v, json!({"status": "timeout", "session": "s"}));
    }

    #[test]
    fn done_builds_the_outcome_from_a_completion_record() {
        let record = json!({"result": "short", "result_full": "long", "duration_ms": 9});
        let o = done("s", &record);
        assert_eq!(o.status, "done");
        assert_eq!(o.session, "s");
        assert_eq!(o.result.as_deref(), Some("long"));
        assert_eq!(o.duration_ms, Some(9));
        assert!(o.detail.is_empty());
    }

    #[test]
    fn result_prefers_the_full_text_and_falls_back_to_the_preview() {
        assert_eq!(
            result_of(&json!({"result": "short", "result_full": "long"})).as_deref(),
            Some("long")
        );
        assert_eq!(
            result_of(&json!({"result": "short"})).as_deref(),
            Some("short")
        );
        assert_eq!(result_of(&json!({"duration_ms": 1})), None);
    }

    #[test]
    fn statuses_map_to_their_exit_codes() {
        assert_eq!(outcome("done").code(), OK);
        assert_eq!(outcome("expired").code(), ERR);
        assert_eq!(outcome("blocked").code(), BLOCKED);
        assert_eq!(outcome("timeout").code(), TIMEOUT);
        assert_eq!(outcome("anything else").code(), ERR);
        assert_ne!(BLOCKED, TIMEOUT);
        assert_ne!(ERR, BLOCKED);
    }

    #[test]
    fn an_expired_watermark_exits_one() {
        let loc = fixture("expired");
        write_completion(&loc);
        // A floor two past the entry generation means the watermark itself
        // is no longer retained.
        let mut index = Index::load(&loc.completions);
        index.floor = index.generation + 2;
        assert!(index.write_atomic(&loc.completions));
        let code = run(&args(&["--session", "s", "--timeout", "0"]), &loc);
        assert_eq!(code, ERR);
    }

    #[test]
    fn parse_reads_pid_cwd_session_timeout_since_and_json() {
        let a = parse(&args(&[
            "--pid",
            "77",
            "--cwd",
            "/work",
            "--session",
            "abc",
            "--timeout",
            "5",
            "--since",
            "123",
            "--json",
        ]));
        assert_eq!(a.filter.pid, Some(77));
        assert_eq!(a.filter.cwd.as_deref(), Some("/work"));
        assert_eq!(a.filter.session.as_deref(), Some("abc"));
        assert_eq!(a.timeout, Duration::from_secs(5));
        assert_eq!(a.since, Some(123_000));
        assert!(a.json);
        let none = parse(&[]);
        assert!(none.filter.is_empty());
        assert_eq!(none.timeout, DEFAULT_TIMEOUT);
        assert!(!none.json && none.since.is_none());
    }

    #[test]
    fn since_reads_whole_and_fractional_seconds_as_milliseconds() {
        assert_eq!(since_ms("123"), Some(123_000));
        assert_eq!(since_ms("123."), Some(123_000));
        assert_eq!(since_ms("123.4"), Some(123_400));
        assert_eq!(since_ms("123.45"), Some(123_450));
        assert_eq!(since_ms("123.456"), Some(123_456));
        // `date +%s.%N`: nanoseconds, cut to the millisecond, never rounded up.
        assert_eq!(since_ms("1790808154.999999999"), Some(1_790_808_154_999));
        for bad in ["", ".5", "-1", "+1", "1e3", "12.3.4", "12.-3", " 1", "x"] {
            assert_eq!(since_ms(bad), None, "{bad:?}");
        }
        assert_eq!(since_ms(&u64::MAX.to_string()), None);
    }

    /// bn-m77, end to end. The previous turn ended at 12:00:00.250 and the
    /// caller marked the next one later in that same second: the finished
    /// turn is not this one's answer, so the wait runs on to its timeout.
    #[test]
    fn a_fractional_since_excludes_a_turn_that_ended_earlier_in_its_second() {
        let loc = fixture("since-same-second");
        let second = crate::event::iso_to_epoch("2026-08-09T12:00:00Z").unwrap();
        let mut index = Index::default();
        index.push_events(
            &[Event {
                ts: "2026-08-09T12:00:00.250Z".into(),
                source: "test",
                session: "s".into(),
                kind: Kind::TurnEnd {
                    duration_ms: Some(7),
                    result: Some("previous".into()),
                    result_full: Some("previous".into()),
                },
            }],
            second,
        );
        assert!(index.write_atomic(&loc.completions));
        let wait = |since: String| {
            run(
                &args(&["--session", "s", "--since", &since, "--timeout", "0"]),
                &loc,
            )
        };
        assert_eq!(wait(format!("{second}.600")), TIMEOUT);
        assert_eq!(wait(format!("{second}.100")), OK);
        // A whole-second mark still cannot tell, and still admits it.
        assert_eq!(wait(second.to_string()), OK);
    }

    #[test]
    fn no_selector_is_a_usage_error() {
        let loc = fixture("usage");
        assert_eq!(run(&args(&["--timeout", "0"]), &loc), USAGE);
    }

    #[test]
    fn cwd_reaches_the_filter() {
        let loc = fixture("cwd");
        write_snapshot(
            &loc,
            r#"[{"session":"a","state":"idle","cwd":"/work/a"},
                {"session":"b","state":"blocked","cwd":"/work/b","detail":"d"}]"#,
        );
        // Only b matches, and b is the blocked one.
        assert_eq!(
            run(&args(&["--cwd", "/work/b", "--timeout", "0"]), &loc),
            BLOCKED
        );
        assert_eq!(
            run(&args(&["--cwd", "/work/a", "--timeout", "0"]), &loc),
            TIMEOUT
        );
    }

    #[test]
    fn pid_reaches_the_filter() {
        let loc = fixture("pid");
        let me = std::process::id();
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let starttime: u64 = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        write_snapshot(
            &loc,
            r#"[{"session":"a","state":"idle"},{"session":"b","state":"blocked"}]"#,
        );
        std::fs::write(
            &loc.register,
            format!(r#"{{"session_id":"b","pid":{me},"starttime":{starttime}}}"#) + "\n",
        )
        .unwrap();
        assert_eq!(
            run(&args(&["--pid", &me.to_string(), "--timeout", "0"]), &loc),
            BLOCKED
        );
    }

    fn emitted(o: &Outcome, as_json: bool) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        o.emit(as_json, &mut out, &mut err);
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn done_prints_only_the_answer_on_stdout() {
        let mut o = outcome("done");
        o.result = Some("the answer".into());
        assert_eq!(emitted(&o, false), ("the answer\n".into(), String::new()));
        o.result = None;
        assert_eq!(emitted(&o, false), (String::new(), String::new()));
    }

    #[test]
    fn other_statuses_print_a_diagnostic_on_stderr_only() {
        let mut o = outcome("blocked");
        o.detail = "needs approval".into();
        assert_eq!(
            emitted(&o, false),
            (String::new(), "agentbus: blocked: needs approval\n".into())
        );
        assert_eq!(
            emitted(&outcome("timeout"), false),
            (String::new(), "agentbus: timeout\n".into())
        );
    }

    #[test]
    fn json_mode_prints_the_object_on_stdout_for_every_status() {
        let (out, err) = emitted(&outcome("timeout"), true);
        assert!(err.is_empty());
        let v: Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v, json!({"status": "timeout", "session": "s"}));
    }
}
