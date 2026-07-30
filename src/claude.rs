//! Claude Code transcript → normalised events.
//!
//! Schema confirmed against a live 2370-line transcript rather than recalled.
//! The useful part is that the signals we currently scrape off the screen are
//! all here as explicit records: `custom-title`/`ai-title` for the name,
//! `last-prompt` for the task, `system/turn_duration` for end-of-turn.
//!
//! What is *not* here is any permission-prompt record — approval is UI state and
//! never reaches the transcript. Blocked detection therefore cannot move off the
//! screen, which is why the plugin keeps its rule table.

use crate::event::{one_line, Event, Kind};
use serde_json::Value;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
}

fn session_of(v: &Value) -> Option<String> {
    s(v, "sessionId").or_else(|| s(v, "session_id"))
}

/// One line of a top-level session transcript.
pub fn normalize(v: &Value, fallback_session: &str) -> Vec<Event> {
    let session = session_of(v).unwrap_or_else(|| fallback_session.to_string());
    let ts = s(v, "timestamp").unwrap_or_default();
    let mk = |kind: Kind| Event {
        ts: ts.clone(),
        source: "claude",
        session: session.clone(),
        kind,
    };
    let mut out = Vec::new();

    match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        // Rank 2 beats rank 1: a title the user typed outranks a generated one.
        "custom-title" => {
            if let Some(t) = s(v, "customTitle") {
                out.push(mk(Kind::Session {
                    title: Some(one_line(&t, 120)),
                    title_rank: 2,
                    cwd: None,
                    model: None,
                }));
            }
        }
        "agent-name" => {
            if let Some(t) = s(v, "agentName") {
                out.push(mk(Kind::Session {
                    title: Some(one_line(&t, 120)),
                    title_rank: 2,
                    cwd: None,
                    model: None,
                }));
            }
        }
        "ai-title" => {
            if let Some(t) = s(v, "aiTitle") {
                out.push(mk(Kind::Session {
                    title: Some(one_line(&t, 120)),
                    title_rank: 1,
                    cwd: None,
                    model: None,
                }));
            }
        }
        // Label, not Prompt: this record is written *after* turn_duration, so
        // reading it as the start of a turn made every finished session look
        // like it had immediately begun working again.
        "last-prompt" => {
            if let Some(t) = s(v, "lastPrompt") {
                out.push(mk(Kind::Label {
                    text: one_line(&t, 160),
                }));
            }
        }
        "system" => {
            if v.get("subtype").and_then(|x| x.as_str()) == Some("turn_duration") {
                out.push(mk(Kind::TurnEnd {
                    duration_ms: v.get("durationMs").and_then(|x| x.as_u64()),
                    result: None,
                }));
            }
        }
        "assistant" => {
            if let Some(c) = s(v, "cwd") {
                out.push(mk(Kind::Session {
                    title: None,
                    title_rank: 0,
                    cwd: Some(c),
                    model: v
                        .pointer("/message/model")
                        .and_then(|x| x.as_str())
                        .map(|x| x.to_string()),
                }));
            }
            if let Some(blocks) = v.pointer("/message/content").and_then(|x| x.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|x| x.as_str()) == Some("tool_use") {
                        if let Some(name) = s(b, "name") {
                            out.push(mk(Kind::Tool { name }));
                        }
                    }
                }
            }
            // Per-message deltas, so the fold must sum rather than replace.
            // Cache reads and writes are reported in their own fields; counting
            // only `input_tokens` undercounts a cached session by orders of
            // magnitude (measured: 684 against a real ~700k-token session).
            let usage = |k: &str| {
                v.pointer(&format!("/message/usage/{k}"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
            };
            // Context is what this message actually carried in — fresh input
            // plus whatever was served from cache. It is a level, so it is
            // reported as-is and replaces rather than accumulates.
            let context = usage("input_tokens")
                + usage("cache_read_input_tokens")
                + usage("cache_creation_input_tokens");
            let outp = usage("output_tokens");
            if context > 0 || outp > 0 {
                out.push(mk(Kind::Tokens {
                    output: outp,
                    output_cumulative: false,
                    context,
                }));
            }
        }
        _ => {}
    }
    out
}

/// Sidecar Claude writes beside each subagent transcript. Written about a second
/// after the subagent starts, so an early read legitimately finds nothing.
pub fn subagent_meta(jsonl: &std::path::Path) -> (String, String) {
    let meta = jsonl.with_extension("meta.json");
    let Ok(txt) = std::fs::read_to_string(meta) else {
        return (String::new(), String::new());
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return (String::new(), String::new());
    };
    (
        s(&v, "agentType").unwrap_or_default(),
        s(&v, "description").unwrap_or_default(),
    )
}

/// One line of a subagent transcript, reported against the parent session.
pub fn normalize_subagent(
    v: &Value,
    parent_session: &str,
    agent_id: &str,
    agent_type: &str,
    description: &str,
) -> Vec<Event> {
    let ts = s(v, "timestamp").unwrap_or_default();
    let mut result = None;
    // The last assistant text is the running answer; when the file stops growing
    // it is the final one. There is no end-of-subagent record to wait for.
    if v.get("type").and_then(|x| x.as_str()) == Some("assistant") {
        if let Some(blocks) = v.pointer("/message/content").and_then(|x| x.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|x| x.as_str()) == Some("text") {
                    if let Some(t) = s(b, "text") {
                        result = Some(one_line(&t, 160));
                    }
                }
            }
        }
    }
    vec![Event {
        ts,
        source: "claude",
        session: parent_session.to_string(),
        kind: Kind::Subagent {
            id: agent_id.to_string(),
            state: "working",
            agent_type: Some(agent_type.to_string()),
            description: Some(description.to_string()),
            result,
        },
    }]
}
