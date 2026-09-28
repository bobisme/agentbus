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
struct Args {
    filter: Filter,
    timeout: Duration,
    json: bool,
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
            "--since" => a.since = next.and_then(|v| v.parse().ok()),
            "--json" => a.json = true,
            _ => {}
        }
        i += 1;
    }
    a
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

    fn print(&self, as_json: bool) {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if as_json {
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
            query::line(&mut out, &serde_json::to_string(&v).unwrap_or_default());
            return;
        }
        match self.status {
            // The answer alone on stdout, so `r=$(agentbus wait …)` is the
            // whole integration for a caller that only wants the text.
            "done" => {
                if let Some(r) = &self.result {
                    query::line(&mut out, r);
                }
            }
            _ => {
                eprintln!(
                    "agentbus: {}{}",
                    self.status,
                    if self.detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", self.detail)
                    }
                );
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
            // Ambiguity is the caller's to resolve and will not clear itself.
            Some(r @ Resolved::Many(_)) => {
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
    if let Some((st, detail)) = state_of(loc, &session) {
        if st == "blocked" {
            return finish(
                Outcome {
                    status: "blocked",
                    session,
                    result: None,
                    duration_ms: None,
                    detail,
                },
                a.json,
            );
        }
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
        if let Some((st, detail)) = state_of(loc, &session) {
            if st == "blocked" {
                return finish(
                    Outcome {
                        status: "blocked",
                        session,
                        result: None,
                        duration_ms: None,
                        detail,
                    },
                    a.json,
                );
            }
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
}
