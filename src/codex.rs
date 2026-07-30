//! Codex rollout → normalised events.
//!
//! Codex has the cleaner stream of the two: explicit `task_started` /
//! `task_complete` turn boundaries, a running token total, and the final message
//! carried on the completion event. Schema confirmed against live rollouts.
//!
//! Codex subagents get their own rollout file rather than a nested directory,
//! and it is the subagent's own `session_meta` that names it, via
//! `agent_nickname`. The file does not record its parent, so a subagent rollout
//! read in isolation cannot be attributed — see the note in main.rs.

use crate::event::{one_line, Event, Kind};
use serde_json::Value;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
}

/// Returns the nickname if this rollout belongs to a subagent.
pub fn nickname(v: &Value) -> Option<String> {
    if v.get("type").and_then(|x| x.as_str()) != Some("session_meta") {
        return None;
    }
    v.pointer("/payload/agent_nickname")
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
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
