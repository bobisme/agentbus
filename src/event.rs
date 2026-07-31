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
    /// The turn is over. `result` is the agent's final message, and is carried
    /// whatever the source: an agent whose end-of-turn record has no text has
    /// it remembered from the turn instead, so that "what did it say" never
    /// sends a subscriber off to find and parse a transcript itself.
    TurnEnd {
        duration_ms: Option<u64>,
        result: Option<String>,
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
            } => {
                o.insert("kind".into(), json!("turn_end"));
                if let Some(d) = duration_ms {
                    o.insert("duration_ms".into(), json!(d));
                }
                if let Some(r) = result {
                    o.insert("result".into(), json!(r));
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
        json!({"version": 1, "sessions": sessions})
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

/// Parse `2026-07-31T00:26:12.774Z` to epoch seconds, ignoring the fraction.
///
/// Hand-rolled because this is the only date handling in the project and it is
/// always this one shape; a dependency for it would cost more than it saves.
fn iso_to_epoch(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |a: usize, z: usize| ts.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from civil, Howard Hinnant's algorithm: correct for any proleptic
    // Gregorian date, and short enough to read.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    u64::try_from(secs).ok()
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
}
