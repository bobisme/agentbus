//! The normalised event vocabulary, and the state it folds into.
//!
//! Everything downstream sees these events, never an agent's native schema.
//! That is the whole point of the split: adding an agent means writing one
//! normaliser, not touching any subscriber.

use serde_json::{json, Value};
use std::collections::BTreeMap;

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
    },
    /// The user asked for something. Doubles as the start-of-turn signal.
    Prompt { text: String },
    TurnEnd {
        duration_ms: Option<u64>,
        result: Option<String>,
    },
    Tool { name: String },
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
    Subagent {
        id: String,
        state: &'static str,
        agent_type: Option<String>,
        description: Option<String>,
        result: Option<String>,
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
            Kind::Session { title, cwd, model, .. } => {
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
            Kind::Prompt { text } => {
                o.insert("kind".into(), json!("prompt"));
                o.insert("text".into(), json!(text));
            }
            Kind::TurnEnd { duration_ms, result } => {
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
            } => {
                o.insert("kind".into(), json!("subagent"));
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
    pub last_tool: String,
    /// Cumulative tokens generated this session.
    pub tokens_out: u64,
    /// Current context occupancy — a level, not a running total.
    pub context: u64,
    pub last_ts: String,
    pub subagents: BTreeMap<String, SubState>,
}

#[derive(Default)]
pub struct Snapshot {
    pub sessions: BTreeMap<String, SessionState>,
}

impl Snapshot {
    pub fn apply(&mut self, e: &Event) {
        let s = self.sessions.entry(e.session.clone()).or_default();
        s.source = e.source.to_string();
        if !e.ts.is_empty() {
            s.last_ts = e.ts.clone();
        }
        match &e.kind {
            Kind::Session {
                title,
                title_rank,
                cwd,
                model,
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
            }
            Kind::Prompt { text } => {
                // An empty prompt is a bare "turn is running" marker (Codex's
                // task_started). It must assert the state without erasing the
                // label the previous real prompt set.
                if !text.is_empty() {
                    s.label = text.clone();
                }
                s.state = "working".into();
            }
            Kind::TurnEnd { .. } => s.state = "idle".into(),
            Kind::Tool { name } => {
                s.last_tool = name.clone();
                s.state = "working".into();
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
            } => {
                let sub = s.subagents.entry(id.clone()).or_default();
                sub.state = state.to_string();
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
                    "last_tool": s.last_tool,
                    "tokens": {"output": s.tokens_out, "context": s.context},
                    "last_activity": s.last_ts,
                    "subagents": subs,
                })
            })
            .collect();
        json!({"version": 1, "sessions": sessions})
    }
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
