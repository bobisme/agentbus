//! The normalised event vocabulary, and the state it folds into.
//!
//! Everything downstream sees these events, never an agent's native schema.
//! That is the whole point of the split: adding an agent means writing one
//! normaliser, not touching any subscriber.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

/// How long a finished subagent's result stays published. Expiry lives here
/// rather than in a subscriber: a subscriber that recomputes "finished" from
/// each snapshot has no way to know how long ago it happened, so every one of
/// them would keep the row forever.
pub const RESULT_TTL: Duration = Duration::from_secs(90);

#[derive(Debug, Clone)]
pub enum Kind {
    /// Identity: what this session is and where it lives.
    Session {
        title: Option<String>,
        /// Which title source this came from, for precedence — a name the user
        /// typed must not be overwritten by a model-generated one.
        title_rank: u8,
        cwd: Option<String>,
        model: Option<String>,
        /// Reasoning effort. Both agents expose it and it changes how a session
        /// behaves as much as the model does, so it belongs beside it.
        effort: Option<String>,
    },
    /// The user asked for something. Doubles as the start-of-turn signal, and
    /// every source owes one: a subscriber waiting for `Prompt` then `TurnEnd`
    /// must not need to know which agent it is watching. Emitting none left
    /// such a wait hanging forever against Claude while working against Codex.
    Prompt {
        text: String,
    },
    /// What the current task is, with no claim about the turn. Claude restates
    /// the last prompt *after* the turn-end record, so treating that restatement
    /// as a new turn left every session pinned to "working" forever.
    Label {
        text: String,
    },
    /// The turn is over, carrying the agent's final message whatever the
    /// source: an agent whose end-of-turn record has no text has it remembered
    /// from the turn instead, so that "what did it say" never sends a
    /// subscriber off to find and parse a transcript itself.
    ///
    /// Two spellings of the same message, because they have different jobs.
    /// `result` is a one-line preview for a status renderer, and lossy by
    /// design. `result_full` is the message, and is what makes the bus a
    /// transport rather than a status feed — without it every consumer that
    /// wanted the answer had to locate the transcript, sniff which agent wrote
    /// it, handle three record shapes, and guard against reading mid-write, all
    /// to recover text this process had already parsed and thrown away.
    TurnEnd {
        duration_ms: Option<u64>,
        /// Collapsed to one line and capped. Safe to render anywhere.
        result: Option<String>,
        /// Untruncated, with its paragraph structure intact. Absent when the
        /// turn genuinely said nothing.
        result_full: Option<String>,
    },
    Tool {
        name: String,
    },
    /// Two genuinely different quantities, which is why there is no single
    /// "total". `output` is work done and accumulates; `context` is how full the
    /// window is right now and does not. Summing `context` across messages
    /// counts the whole conversation once per message — measured 1.35 billion
    /// on a real session — and summing only fresh input undercounts by as much.
    Tokens {
        output: u64,
        /// True when `output` is already a running total (Codex) rather than a
        /// per-message delta (Claude).
        output_cumulative: bool,
        context: u64,
    },
    /// State asserted by an agent's own integration, for agents whose state
    /// reaches no file we can read. Unlike the derived states this may say
    /// "blocked", which no transcript ever records.
    Reported {
        state: String,
        detail: String,
    },
    Subagent {
        id: String,
        state: &'static str,
        agent_type: Option<String>,
        description: Option<String>,
        result: Option<String>,
        /// A subagent may run a different model or effort from its parent,
        /// which is exactly when knowing is useful.
        model: Option<String>,
        effort: Option<String>,
        /// A tool the subagent just invoked, counted the same way a session's
        /// are. Subagents write their own transcripts, so this is observed
        /// rather than reported.
        tool: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Event {
    pub ts: String,
    pub source: &'static str,
    pub session: String,
    pub kind: Kind,
}

impl Event {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "ts": self.ts,
            "source": self.source,
            "session": self.session,
        });
        let o = v.as_object_mut().unwrap();
        match &self.kind {
            Kind::Session {
                title, cwd, model, ..
            } => {
                o.insert("kind".into(), json!("session"));
                if let Some(t) = title {
                    o.insert("title".into(), json!(t));
                }
                if let Some(c) = cwd {
                    o.insert("cwd".into(), json!(c));
                }
                if let Some(m) = model {
                    o.insert("model".into(), json!(m));
                }
            }
            Kind::Reported { state, detail } => {
                o.insert("kind".into(), json!("reported"));
                o.insert("state".into(), json!(state));
                o.insert("detail".into(), json!(detail));
            }
            Kind::Prompt { text } => {
                o.insert("kind".into(), json!("prompt"));
                o.insert("text".into(), json!(text));
            }
            Kind::Label { text } => {
                o.insert("kind".into(), json!("label"));
                o.insert("text".into(), json!(text));
            }
            Kind::TurnEnd {
                duration_ms,
                result,
                result_full,
            } => {
                o.insert("kind".into(), json!("turn_end"));
                if let Some(d) = duration_ms {
                    o.insert("duration_ms".into(), json!(d));
                }
                if let Some(r) = result {
                    o.insert("result".into(), json!(r));
                }
                // Emitted even when identical to the preview. A consumer that
                // has to check whether the field is present before deciding
                // which to read is back to special-casing, which is what
                // carrying it at all was meant to end.
                if let Some(r) = result_full {
                    o.insert("result_full".into(), json!(r));
                }
            }
            Kind::Tool { name } => {
                o.insert("kind".into(), json!("tool"));
                o.insert("name".into(), json!(name));
            }
            Kind::Tokens {
                output, context, ..
            } => {
                o.insert("kind".into(), json!("tokens"));
                o.insert("output".into(), json!(output));
                o.insert("context".into(), json!(context));
            }
            Kind::Subagent {
                id,
                state,
                agent_type,
                description,
                result,
                model,
                effort,
                tool,
            } => {
                o.insert("kind".into(), json!("subagent"));
                if let Some(m) = model {
                    o.insert("model".into(), json!(m));
                }
                if let Some(e) = effort {
                    o.insert("effort".into(), json!(e));
                }
                if let Some(t) = tool {
                    o.insert("tool".into(), json!(t));
                }
                o.insert("id".into(), json!(id));
                o.insert("state".into(), json!(state));
                if let Some(t) = agent_type {
                    o.insert("agent_type".into(), json!(t));
                }
                if let Some(d) = description {
                    o.insert("description".into(), json!(d));
                }
                if let Some(r) = result {
                    o.insert("result".into(), json!(r));
                }
            }
        }
        v
    }
}

#[derive(Default, Debug, Clone)]
pub struct SubState {
    pub agent_type: String,
    pub description: String,
    pub result: String,
    pub state: String,
    pub model: String,
    pub effort: String,
    pub tools: u64,
    pub last_tool: String,
    /// Epoch seconds of the earliest record seen for this subagent. Its start
    /// hook carries no timestamp, so this comes from its transcript.
    pub started: u64,
    /// When it finished, for expiry. Set once, on the transition.
    pub done_since: Option<SystemTime>,
}

#[derive(Default, Debug, Clone)]
pub struct SessionState {
    pub source: String,
    pub title: String,
    pub title_rank: u8,
    pub label: String,
    /// "working" or "idle". Deliberately no "done": an agent that finished is
    /// one you have not given the next thing to yet.
    pub state: String,
    pub cwd: String,
    pub model: String,
    pub effort: String,
    pub last_tool: String,
    /// Tool calls in the current turn. Per turn rather than per session: the
    /// question a roster answers is "what is it doing now", and a lifetime
    /// total only grows.
    pub tool_calls: u64,
    /// When the current state began, epoch seconds. Published rather than an
    /// elapsed count, because the snapshot is only rewritten when something
    /// changes — an elapsed number would freeze between changes.
    pub state_since: u64,
    /// Free text accompanying a reported state, e.g. what permission is being
    /// asked for.
    pub detail: String,
    /// Cumulative tokens generated this session.
    pub tokens_out: u64,
    /// Current context occupancy — a level, not a running total.
    pub context: u64,
    pub last_ts: String,
    /// Where this session lives, from the register. Absent until a hook has
    /// reported it, or once the reporting process is gone.
    pub mux: String,
    pub mux_session: String,
    pub pane: String,
    pub subagents: BTreeMap<String, SubState>,
}

#[derive(Default)]
pub struct Snapshot {
    pub sessions: BTreeMap<String, SessionState>,
    /// True while replaying history at startup. Transitions seen then happened
    /// in the past, so they may only be timed from a record that says when —
    /// never from the clock, which would date them all to startup.
    pub backfilling: bool,
}

impl Snapshot {
    pub fn apply(&mut self, e: &Event) {
        // When this happened. Transcript records carry a timestamp; hook reports
        // do not, so live ones are dated now and replayed ones are left undated
        // rather than guessed at.
        let at = iso_to_epoch(&e.ts).or(if self.backfilling {
            None
        } else {
            Some(now_secs())
        });
        let s = self.sessions.entry(e.session.clone()).or_default();
        // `source` names the agent, not whatever last spoke about it. A hook
        // report is a transport; letting it overwrite made a Claude session
        // read as "hook" the moment one arrived, and anything keyed on the
        // agent then skipped it.
        if e.source != "hook" || s.source.is_empty() {
            s.source = e.source.to_string();
        }
        if !e.ts.is_empty() {
            s.last_ts = e.ts.clone();
        }
        match &e.kind {
            Kind::Session {
                title,
                title_rank,
                cwd,
                model,
                effort,
            } => {
                // Only take a title at least as authoritative as what we hold,
                // so a model-written title can't clobber a user-set one.
                if let Some(t) = title {
                    if *title_rank >= s.title_rank && !t.is_empty() {
                        s.title = t.clone();
                        s.title_rank = *title_rank;
                    }
                }
                if let Some(c) = cwd {
                    s.cwd = c.clone();
                }
                if let Some(m) = model {
                    s.model = m.clone();
                }
                if let Some(e) = effort {
                    s.effort = e.clone();
                }
            }
            Kind::Prompt { text } => {
                // An empty prompt is a bare "turn is running" marker (Codex's
                // task_started). It must assert the state without erasing the
                // label the previous real prompt set.
                if !text.is_empty() {
                    s.label = text.clone();
                    s.tool_calls = 0;
                    // A real new prompt starts a new turn, so last turn's
                    // finished subagents stop being interesting.
                    s.subagents.retain(|_, sub| sub.done_since.is_none());
                }
                set_state(s, "working", at);
            }
            Kind::Label { text } => {
                if !text.is_empty() {
                    s.label = text.clone();
                }
            }
            Kind::TurnEnd { .. } => set_state(s, "idle", at),
            Kind::Reported { state, detail } => {
                // Integrations still say "done" when a turn ends. That is the
                // idle case — finished, awaiting whatever you ask next — so it
                // is normalised here rather than leaking a fourth state onto
                // the bus for every subscriber to special-case.
                if !state.is_empty() {
                    set_state(s, if state == "done" { "idle" } else { state }, at);
                }
                s.detail = detail.clone();
            }
            Kind::Tool { name } => {
                s.last_tool = name.clone();
                s.tool_calls += 1;
                set_state(s, "working", at);
            }
            Kind::Tokens {
                output,
                output_cumulative,
                context,
            } => {
                if *output_cumulative {
                    // Take the max rather than assigning, so a stale line
                    // arriving late cannot rewind the counter.
                    s.tokens_out = s.tokens_out.max(*output);
                } else {
                    s.tokens_out += output;
                }
                if *context > 0 {
                    s.context = *context;
                }
            }
            Kind::Subagent {
                id,
                state,
                agent_type,
                description,
                result,
                model,
                effort,
                tool,
            } => {
                let sub = s.subagents.entry(id.clone()).or_default();
                if let Some(m) = model {
                    sub.model = m.clone();
                }
                if let Some(e) = effort {
                    sub.effort = e.clone();
                }
                if let Some(t) = tool {
                    sub.tools += 1;
                    sub.last_tool = t.clone();
                }
                // Earliest record wins: a subagent's own transcript is the only
                // thing that says when it started, since the hook that
                // announces it carries no timestamp.
                if let Some(t) = at {
                    if sub.started == 0 || t < sub.started {
                        sub.started = t;
                    }
                }
                // Stamp the transition, not every repeat, or the clock resets
                // each time the same completion is seen and it never expires.
                if *state == "done" && sub.done_since.is_none() {
                    sub.done_since = Some(SystemTime::now());
                }
                // An empty state means "no claim" — used by sources that can
                // name or describe a subagent but do not witness its lifecycle.
                if !state.is_empty() {
                    sub.state = state.to_string();
                }
                if let Some(t) = agent_type {
                    sub.agent_type = t.clone();
                }
                // Only ever upgrade: a later event with a blank description must
                // not erase one we already resolved.
                if let Some(d) = description {
                    if !d.is_empty() {
                        sub.description = d.clone();
                    }
                }
                if let Some(r) = result {
                    if !r.is_empty() {
                        sub.result = r.clone();
                    }
                }
            }
        }
    }

    /// Drop finished subagents whose results have been published long enough
    /// to read. Called on the publish tick, so subscribers simply stop seeing
    /// them rather than each having to age them out.
    pub fn expire(&mut self, ttl: Duration) {
        let now = SystemTime::now();
        for s in self.sessions.values_mut() {
            s.subagents.retain(|_, sub| match sub.done_since {
                Some(t) => now.duration_since(t).unwrap_or_default() < ttl,
                None => true,
            });
        }
    }

    pub fn to_json(&self) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|(id, s)| {
                let subs: Vec<Value> = s
                    .subagents
                    .iter()
                    .map(|(sid, sub)| {
                        json!({
                            "id": sid,
                            "state": sub.state,
                            "agent_type": sub.agent_type,
                            "description": sub.description,
                            "result": sub.result,
                            "model": sub.model,
                            "effort": sub.effort,
                            "tools": sub.tools,
                            "last_tool": sub.last_tool,
                            "started": sub.started,
                        })
                    })
                    .collect();
                json!({
                    "session": id,
                    "source": s.source,
                    "title": s.title,
                    "label": s.label,
                    "state": s.state,
                    "cwd": s.cwd,
                    "model": s.model,
                    "effort": s.effort,
                    "last_tool": s.last_tool,
                    "tools": s.tool_calls,
                    "state_since": s.state_since,
                    "detail": s.detail,
                    "tokens": {"output": s.tokens_out, "context": s.context},
                    "last_activity": s.last_ts,
                    "location": {"mux": s.mux, "session": s.mux_session, "pane": s.pane},
                    "subagents": subs,
                })
            })
            .collect();
        json!({"version": SNAPSHOT_VERSION, "sessions": sessions})
    }
}

/// Stamp when a state began, but only on an actual change. Re-stamping on every
/// restatement would make a long-running turn permanently read as just started.
///
/// `at` of None means the moment is unknown — a replayed report with no
/// timestamp of its own. The state still changes; only the clock stays silent,
/// because a subscriber showing the wrong duration is worse than one showing
/// none.
fn set_state(s: &mut SessionState, to: &str, at: Option<u64>) {
    if s.state != to {
        s.state = to.to_string();
        s.state_since = at.unwrap_or(0);
        return;
    }
    // Same state, but we never learned when it began — the transition was an
    // undated report. The earliest dated record confirming it is not when it
    // started, but it is a bound, and it beats showing nothing at all for a
    // session that will not change state again for hours.
    if s.state_since == 0 {
        if let Some(t) = at {
            s.state_since = t;
        }
    }
}

/// Schema of the published snapshot.
///
/// 2 — `location: {mux, session, pane}`, replacing 1's `pane: {zellij_session,
///     pane_id}`. The rename is what forced the bump: `mux` is the necessary
///     field now that tmux, wezterm and kitty are recognised and a subscriber
///     has to know which it is looking at.
///
/// Bump this whenever a field is renamed, removed, or changes meaning. Purely
/// additive fields do not — `state_since`, `tools` and `effort` all arrived
/// under 2 without one, and expressing that difference is what the number is
/// for. A subscriber should accept the versions it knows and warn on anything
/// else; the failure this exists to prevent is the silent one, where reading a
/// renamed field yields nothing, no session ever matches, and the agent looks
/// unresponsive rather than the schema looking wrong.
pub const SNAPSHOT_VERSION: u64 = 2;

/// Parse `2026-07-31T00:26:12.774Z` to epoch seconds, ignoring everything
/// after byte 19 (the fraction and the zone designator).
///
/// Hand-rolled because this is the only date handling in the project and it is
/// always this one shape; a dependency for it would cost more than it saves.
///
/// The first 19 bytes are validated strictly: ASCII digits only (no sign, no
/// whitespace, which `str::parse` would let through), separators `-`, `-`, `T`,
/// `:`, `:` at bytes 4, 7, 10, 13, 16, month 1-12, a day that exists in that
/// month (Gregorian leap years), hour < 24, minute < 60, second < 60. A leap
/// second (`:60`) is rejected on purpose: Unix time has no representation for
/// it, so accepting it would map it onto the following `:00` and make two
/// distinct timestamps compare equal. Dates before 1970 are also `None`.
///
/// Proved by the Kani harnesses at the bottom of this file (`just kani`).
pub fn iso_to_epoch(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    // Fixed-width ASCII digits by hand: `str::parse::<i64>` accepts a leading
    // `+` or `-`, which would let `+026` pass as a year.
    let n = |a: usize, z: usize| -> Option<u32> {
        let mut v = 0u32;
        for &c in &b[a..z] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + u32::from(c - b'0');
        }
        Some(v)
    };
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_month = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if d < 1 || d > days_in_month || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    let secs = i64::from(days) * 86400 + i64::from(h * 3600 + mi * 60 + sec);
    u64::try_from(secs).ok()
}

/// Days since 1970-01-01 for a valid proleptic Gregorian date with year
/// 0..=9999. Howard Hinnant's days-from-civil algorithm, correct for any such
/// date and short enough to read. Shifted forward one 400-year era so every
/// term is unsigned (Hinnant's original branches on a negative year); the
/// shift is subtracted again at the end. This also keeps it tractable for
/// Kani, which did not finish on the signed 64-bit form.
fn days_from_civil(y: u32, mo: u32, d: u32) -> i32 {
    let y = if mo <= 2 { y + 399 } else { y + 400 };
    let (era, yoe) = (y / 400, y % 400);
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    // At most 2.9 million in magnitude for year <= 9999, so it fits i32.
    (era * 146097 + doe) as i32 - 719468 - 146097
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Collapse to a single line and cap. Control characters are removed, not
/// replaced: this text reaches a terminal renderer that uses escape codes for
/// colour, and an ESC in model output could rewrite everything after it.
pub fn one_line(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        collapsed.chars().take(max).collect::<String>() + "…"
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::iso_to_epoch;

    /// Expected values from `date -d '<ts>' +%s`. This is the only date handling
    /// in the project, and a wrong answer here shows a plausible but incorrect
    /// duration rather than failing visibly.
    #[test]
    fn parses_transcript_timestamps() {
        assert_eq!(iso_to_epoch("2026-07-31T00:26:12.774Z"), Some(1785457572));
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_to_epoch("2000-03-01T00:00:00Z"), Some(951868800));
        // Leap day, the case the month-shifting exists for.
        assert_eq!(iso_to_epoch("2024-02-29T12:00:00Z"), Some(1709208000));
    }

    #[test]
    fn rejects_anything_else() {
        assert_eq!(iso_to_epoch(""), None);
        assert_eq!(iso_to_epoch("not a date"), None);
        assert_eq!(iso_to_epoch("2026-07-31"), None);
    }

    #[test]
    fn rejects_out_of_range_and_malformed_fields() {
        assert_eq!(iso_to_epoch("2026-13-01T00:00:00Z"), None); // month 13
        assert_eq!(iso_to_epoch("2026-00-10T00:00:00Z"), None); // month 0
        assert_eq!(iso_to_epoch("2026-07-32T00:00:00Z"), None); // day 32
        assert_eq!(iso_to_epoch("2026-07-00T00:00:00Z"), None); // day 0
        assert_eq!(iso_to_epoch("2026-04-31T00:00:00Z"), None); // April has 30
        assert_eq!(iso_to_epoch("2026-07-31T24:00:00Z"), None); // hour 24
        assert_eq!(iso_to_epoch("2026-07-31T00:60:00Z"), None); // minute 60
        assert_eq!(iso_to_epoch("2026-07-31T00:00:60Z"), None); // leap second
        assert_eq!(iso_to_epoch("2025-02-29T00:00:00Z"), None); // not a leap year
        assert_eq!(iso_to_epoch("2100-02-29T00:00:00Z"), None); // century, not leap
        assert_eq!(iso_to_epoch("+026-07-31T00:00:00Z"), None); // sign in year
        assert_eq!(iso_to_epoch("2026-+7-31T00:00:00Z"), None);
        assert_eq!(iso_to_epoch("2026-07-31T 0:00:00Z"), None); // whitespace
        assert_eq!(iso_to_epoch("2026-07-31T00-00:00Z"), None); // missing ':'
        assert_eq!(iso_to_epoch("2026-07-31T00:00-00Z"), None); // missing ':'
        assert_eq!(iso_to_epoch("2026-07-31T00:00:0éZ"), None); // non-ASCII
    }

    #[test]
    fn accepts_leap_days() {
        assert_eq!(iso_to_epoch("2000-02-29T00:00:00Z"), Some(951782400));
        assert_eq!(iso_to_epoch("2024-02-29T23:59:59Z"), Some(1709251199));
    }
}

// Kani proofs. `iso_to_epoch` takes a fixed 19-byte prefix, so these cover
// every input completely (the only loops are the 2-4 digit scans).
#[cfg(kani)]
mod kani_proofs {
    use super::iso_to_epoch;

    fn digit(b: u8) -> u32 {
        u32::from(b - b'0')
    }

    /// View arbitrary bytes as a `&str` without UTF-8 validation. `from_utf8`
    /// makes CBMC intractable (validation of 24 symbolic bytes did not finish
    /// in 8 minutes). This is sound here because `iso_to_epoch` only ever calls
    /// `str::as_bytes` and never a char-based `str` API, so it cannot observe
    /// invalid UTF-8; the inputs cover a strict superset of valid strings.
    fn as_str(bytes: &[u8]) -> &str {
        // SAFETY: see above; the result is only used through `as_bytes`.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    fn field(b: &[u8], a: usize, z: usize) -> u32 {
        b[a..z].iter().fold(0, |v, &c| v * 10 + digit(c))
    }

    /// Any byte string of 19..=24 bytes (a superset of the valid UTF-8 ones): never panics.
    #[kani::proof]
    #[kani::unwind(20)]
    fn iso_to_epoch_never_panics() {
        let bytes: [u8; 24] = kani::any();
        let len: usize = kani::any();
        kani::assume((19..=24).contains(&len));
        let _ = iso_to_epoch(as_str(&bytes[..len]));
    }

    /// `Some(_)` implies every field of the 19-byte prefix is well-formed.
    /// Second 60 is excluded deliberately (see the function docs).
    #[kani::proof]
    #[kani::unwind(20)]
    fn iso_to_epoch_some_implies_well_formed() {
        let bytes: [u8; 24] = kani::any();
        let len: usize = kani::any();
        kani::assume((19..=24).contains(&len));
        if iso_to_epoch(as_str(&bytes[..len])).is_some() {
            let b = &bytes[..19];
            for (i, &c) in b.iter().enumerate() {
                if matches!(i, 4 | 7 | 10 | 13 | 16) {
                    let want = if i == 10 {
                        b'T'
                    } else if i < 10 {
                        b'-'
                    } else {
                        b':'
                    };
                    assert!(c == want);
                } else {
                    assert!(c.is_ascii_digit()); // no sign, no whitespace
                }
            }
            let (y, mo, d) = (field(b, 0, 4), field(b, 5, 7), field(b, 8, 10));
            let (h, mi, sec) = (field(b, 11, 13), field(b, 14, 16), field(b, 17, 19));
            assert!((1..=12).contains(&mo));
            let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
            let dim = match mo {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                _ if leap => 29,
                _ => 28,
            };
            assert!(d >= 1 && d <= dim);
            assert!(h < 24 && mi < 60 && sec < 60);
        }
    }

    fn days_in_month(y: u32, mo: u32) -> u32 {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        match mo {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ if leap => 29,
            _ => 28,
        }
    }

    fn render([y, mo, d, h, mi, sec]: [u32; 6]) -> [u8; 19] {
        let dg = |v: u32, div: u32| b'0' + ((v / div) % 10) as u8;
        [
            dg(y, 1000),
            dg(y, 100),
            dg(y, 10),
            dg(y, 1),
            b'-',
            dg(mo, 10),
            dg(mo, 1),
            b'-',
            dg(d, 10),
            dg(d, 1),
            b'T',
            dg(h, 10),
            dg(h, 1),
            b':',
            dg(mi, 10),
            dg(mi, 1),
            b':',
            dg(sec, 10),
            dg(sec, 1),
        ]
    }

    /// The converse of harness 2, so over-rejection is caught too: every
    /// well-formed timestamp from 1970 on is accepted.
    #[kani::proof]
    #[kani::unwind(20)]
    fn well_formed_is_accepted() {
        let [y, mo, d, h, mi, sec]: [u32; 6] = [(); 6].map(|_| kani::any());
        kani::assume((1970..=9999).contains(&y) && (1..=12).contains(&mo));
        kani::assume((1..=days_in_month(y, mo)).contains(&d) && h < 24 && mi < 60 && sec < 60);
        assert!(iso_to_epoch(as_str(&render([y, mo, d, h, mi, sec]))).is_some());
    }

    /// Ordering, proved by decomposition. Checking two symbolic timestamps
    /// directly (`a < b` iff `epoch(a) < epoch(b)`) did not finish in ten
    /// minutes: it needs CBMC to relate two copies of the days-from-civil
    /// arithmetic. The property is split into three cheaper facts about the
    /// real code, and the composition is on paper below.
    ///
    /// Let t range over well-formed timestamps from 1970-01-01T00:00:00 to
    /// 9999-12-31T23:59:59, and `next(t)` be the calendar second after t.
    ///   (A) `days_successor`: days(next date) == days(date) + 1.
    ///   (B) `epoch_is_days_plus_time_of_day`: epoch(t) == days*86400 + tod.
    ///   (C) `successor_bytes_are_greater`: bytes(t) < bytes(next(t)).
    /// From (A) and (B): within a day epoch(next) = epoch + 1 (tod + 1); across
    /// midnight epoch(next) = (days+1)*86400 + 0 = days*86400 + 86399 + 1 =
    /// epoch + 1. So epoch(next(t)) == epoch(t) + 1 always. The timestamps form
    /// one chain under `next`, and by (C) with fixed-width digits (which order
    /// as their fields do) byte order is chain order. For a < b bytewise, b is
    /// k >= 1 steps after a, so epoch(b) = epoch(a) + k > epoch(a); symmetric
    /// for a > b; a == b gives equal epochs. Byte order is total, so the three
    /// cases are exhaustive and `==` and `<` are both iff. Timestamps before
    /// 1970 are `None` by design and out of scope.
    fn next_date(y: u32, mo: u32, d: u32) -> (u32, u32, u32) {
        if d < days_in_month(y, mo) {
            (y, mo, d + 1)
        } else if mo < 12 {
            (y, mo + 1, 1)
        } else {
            (y + 1, 1, 1)
        }
    }

    #[kani::proof]
    fn days_successor() {
        let (y, mo, d): (u32, u32, u32) = (kani::any(), kani::any(), kani::any());
        kani::assume((1970..=9999).contains(&y) && (1..=12).contains(&mo));
        kani::assume((1..=days_in_month(y, mo)).contains(&d));
        // The last day's successor is year 10000, outside days_from_civil's domain.
        kani::assume(!(y == 9999 && mo == 12 && d == 31));
        let (y2, mo2, d2) = next_date(y, mo, d);
        assert!(super::days_from_civil(y2, mo2, d2) == super::days_from_civil(y, mo, d) + 1);
    }

    #[kani::proof]
    #[kani::unwind(20)]
    fn epoch_is_days_plus_time_of_day() {
        let [y, mo, d, h, mi, sec]: [u32; 6] = [(); 6].map(|_| kani::any());
        kani::assume((1970..=9999).contains(&y) && (1..=12).contains(&mo));
        kani::assume((1..=days_in_month(y, mo)).contains(&d) && h < 24 && mi < 60 && sec < 60);
        let t = render([y, mo, d, h, mi, sec]);
        let want = i64::from(super::days_from_civil(y, mo, d)) * 86400
            + i64::from(h * 3600 + mi * 60 + sec);
        assert!(iso_to_epoch(as_str(&t)) == Some(want as u64));
    }

    #[kani::proof]
    #[kani::unwind(20)]
    fn successor_bytes_are_greater() {
        let [y, mo, d, h, mi, sec]: [u32; 6] = [(); 6].map(|_| kani::any());
        kani::assume((1970..=9999).contains(&y) && (1..=12).contains(&mo));
        kani::assume((1..=days_in_month(y, mo)).contains(&d) && h < 24 && mi < 60 && sec < 60);
        kani::assume(!(y == 9999 && mo == 12 && d == 31 && h == 23 && mi == 59 && sec == 59));
        let next = if sec < 59 {
            [y, mo, d, h, mi, sec + 1]
        } else if mi < 59 {
            [y, mo, d, h, mi + 1, 0]
        } else if h < 23 {
            [y, mo, d, h + 1, 0, 0]
        } else {
            let (y2, mo2, d2) = next_date(y, mo, d);
            [y2, mo2, d2, 0, 0, 0]
        };
        assert!(render([y, mo, d, h, mi, sec]) < render(next));
    }
}
