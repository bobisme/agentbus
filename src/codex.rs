//! Codex rollout → normalised events.
//!
//! Codex has the cleaner stream of the two: explicit `task_started` /
//! `task_complete` turn boundaries, a running token total, and the final message
//! carried on the completion event. Schema confirmed against live rollouts.
//!
//! Codex subagents get their own rollout file rather than a nested directory.
//! Its `session_meta` names it via `agent_nickname` — and records its *parent's*
//! id in `session_id`, with its own in `id`. That asymmetry is a trap: read the
//! usual way, a subagent's prompts get filed under the parent and overwrite the
//! parent's label with whatever the subagent was told to do.

use crate::event::{one_line, Event, Kind};
use serde_json::Value;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
}

/// If this line marks a rollout as a subagent's, returns (parent, own id, name).
///
/// The trap is that a subagent's `session_meta.payload.session_id` is its
/// *parent's* id, with its own in a separate `id` field. Reading session_id the
/// usual way therefore files the subagent's prompts under the parent and
/// overwrites the parent's label with whatever the subagent was told to do.
pub fn subagent_of(v: &Value) -> Option<(String, String, String)> {
    if v.get("type").and_then(|x| x.as_str()) != Some("session_meta") {
        return None;
    }
    let p = |k: &str| {
        v.pointer(&format!("/payload/{k}"))
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
    };
    let nick = p("agent_nickname")?;
    Some((p("session_id")?, p("id")?, nick))
}

/// One line of a subagent's own rollout, reported against its parent.
///
/// Never asserts a state: the parent's hooks witness the lifecycle and report
/// start and stop, and a rollout line arriving afterwards must not un-finish a
/// subagent that has already completed.
pub fn normalize_subagent(v: &Value, parent: &str, agent_id: &str, nickname: &str) -> Vec<Event> {
    let ts = s(v, "timestamp").unwrap_or_default();
    let mut description = None;
    let mut result = None;

    if v.get("type").and_then(|x| x.as_str()) != Some("session_meta") {
        let payload = v.get("payload").unwrap_or(&Value::Null);
        match payload.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            // The first user message that is not an XML-ish preamble is the task.
            "user_message" => {
                if let Some(t) = s(payload, "message") {
                    if !t.trim_start().starts_with('<') {
                        description = Some(one_line(&t, 120));
                    }
                }
            }
            "task_complete" => {
                result = s(payload, "last_agent_message").map(|t| one_line(&t, 160));
            }
            _ => return Vec::new(),
        }
        if description.is_none() && result.is_none() {
            return Vec::new();
        }
    }

    vec![Event {
        ts,
        source: "codex",
        session: parent.to_string(),
        kind: Kind::Subagent {
            id: agent_id.to_string(),
            state: "",
            agent_type: Some(nickname.to_string()),
            description,
            result,
        },
    }]
}

pub fn normalize(v: &Value, fallback_session: &str) -> Vec<Event> {
    let session = v
        .pointer("/payload/session_id")
        .and_then(|x| x.as_str())
        .map(|x| x.to_string())
        .unwrap_or_else(|| fallback_session.to_string());
    let ts = s(v, "timestamp").unwrap_or_default();
    let mk = |kind: Kind| Event {
        ts: ts.clone(),
        source: "codex",
        session: session.clone(),
        kind,
    };
    let mut out = Vec::new();

    if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
        out.push(mk(Kind::Session {
            // Codex has no session title of its own; the roster falls back to
            // the prompt, which is why the label matters more here.
            title: None,
            title_rank: 0,
            cwd: v
                .pointer("/payload/cwd")
                .and_then(|x| x.as_str())
                .map(|x| x.to_string()),
            model: v
                .pointer("/payload/model_provider")
                .and_then(|x| x.as_str())
                .map(|x| x.to_string()),
        }));
        return out;
    }

    let payload = v.get("payload").unwrap_or(&Value::Null);
    match payload.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        "user_message" => {
            if let Some(t) = s(payload, "message") {
                // Codex replays context blocks as user messages; those are
                // XML-ish preambles, not something the user typed.
                if !t.trim_start().starts_with('<') {
                    out.push(mk(Kind::Prompt {
                        text: one_line(&t, 160),
                    }));
                }
            }
        }
        "task_started" => {
            out.push(mk(Kind::Prompt {
                // No text: this only asserts the turn is running. An empty
                // prompt would blank the label, so carry the marker instead.
                text: String::new(),
            }));
        }
        "task_complete" => {
            out.push(mk(Kind::TurnEnd {
                duration_ms: payload.get("duration_ms").and_then(|x| x.as_u64()),
                result: s(payload, "last_agent_message").map(|t| one_line(&t, 160)),
            }));
        }
        "token_count" => {
            let at = |base: &str, k: &str| {
                payload
                    .pointer(&format!("/info/{base}/{k}"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
            };
            // `total_token_usage` is a running total across the session;
            // `last_token_usage` describes only the most recent request, which
            // is what actually indicates current context occupancy.
            let output = at("total_token_usage", "output_tokens");
            let context = at("last_token_usage", "input_tokens")
                + at("last_token_usage", "cached_input_tokens");
            if output > 0 || context > 0 {
                out.push(mk(Kind::Tokens {
                    output,
                    output_cumulative: true,
                    context,
                }));
            }
        }
        "custom_tool_call" => {
            if let Some(name) = s(payload, "tool_name").or_else(|| s(payload, "name")) {
                out.push(mk(Kind::Tool { name }));
            }
        }
        _ => {}
    }
    out
}
