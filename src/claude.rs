//! Claude Code transcript → normalised events.
//!
//! Schema confirmed against a live 2370-line transcript rather than recalled.
//! The useful part is that the signals we currently scrape off the screen are
//! all here as explicit records: `custom-title`/`ai-title` for the name,
//! `last-prompt` for the task, `system/turn_duration` for end-of-turn.
//!
//! Both turn boundaries are here too, but neither is labelled as one. The start
//! is a `user` record — among hundreds of `user` records per turn that are not
//! prompts at all, since tool results, slash-command stdout and interruptions
//! are all written as user messages. The end is `system/turn_duration`, which
//! carries a duration and nothing else, so the turn's answer has to be kept from
//! the assistant message before it. Counted on one real transcript: 801 tool
//! results against 76 actual prompts.
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

/// What one transcript's normaliser has to carry between lines.
///
/// Claude ends a turn with a record holding nothing but a duration, so the
/// answer has to be remembered from the assistant message before it. Codex
/// carries its final message on the completion event itself; keeping this is
/// what stops `turn_end` meaning two different things depending on which agent
/// produced it.
#[derive(Default)]
pub struct Turn {
    /// Most recent non-empty assistant text since the last turn boundary.
    answer: Option<String>,
}

/// The text of a `user` record that genuinely starts a turn, if it is one.
///
/// Most user records are not prompts, and there are hundreds of them per turn:
/// every tool result is written as a user message, so are a slash command's
/// caveat and its stdout, and an interrupted request leaves a marker of its own.
/// Reading any of those as a turn start would pin the session to "working" and
/// overwrite its label with machinery — the same mistake that made `last-prompt`
/// a `Label` rather than a `Prompt`.
fn user_prompt(v: &Value) -> Option<String> {
    let flag = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    // Written by the harness rather than typed: local-command caveats, the
    // placeholder standing in for a pasted image.
    if flag("isMeta") {
        return None;
    }
    // A subagent's own turn, which belongs to the subagent's row.
    if flag("isSidechain") {
        return None;
    }
    let text = match v.pointer("/message/content") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(blocks)) => {
            let is = |b: &Value, t: &str| b.get("type").and_then(|x| x.as_str()) == Some(t);
            // A tool result is a user message too, and is the overwhelming
            // majority of them. Never a prompt, whatever else it carries.
            if blocks.iter().any(|b| is(b, "tool_result")) {
                return None;
            }
            blocks
                .iter()
                .filter(|b| is(b, "text"))
                .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => return None,
    };
    let text = text.trim();
    // Slash-command machinery arrives as user text — <command-name>,
    // <local-command-stdout>. The Codex normaliser draws the same line around
    // its replayed context blocks, for the same reason.
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    // What pressing escape writes. The real prompt follows it on the next line,
    // so treating this one as the turn start would label the turn with it.
    if text.starts_with("[Request interrupted by user") {
        return None;
    }
    Some(text.to_string())
}

/// One line of a top-level session transcript.
pub fn normalize(v: &Value, fallback_session: &str, turn: &mut Turn) -> Vec<Event> {
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
        // The turn start. Without one, a subscriber that follows events rather
        // than polling the snapshot has no way to know a Claude turn began: a
        // wait written as "see a prompt, then wait for turn_end" ran fine
        // against Codex and hung forever here. `last-prompt` cannot serve, since
        // it is written *after* the turn ends — this record is the one that
        // actually opens it.
        "user" => {
            if let Some(t) = user_prompt(v) {
                // Nothing the previous turn said is still pending. An
                // interrupted turn never gets a turn_duration, and its
                // half-answer must not surface as this turn's result.
                turn.answer = None;
                out.push(mk(Kind::Prompt {
                    text: one_line(&t, 160),
                }));
            }
        }
        "system" => {
            if v.get("subtype").and_then(|x| x.as_str()) == Some("turn_duration") {
                out.push(mk(Kind::TurnEnd {
                    duration_ms: v.get("durationMs").and_then(|x| x.as_u64()),
                    // This record carries no text, so the answer comes from the
                    // assistant message that preceded it. Taken rather than
                    // read, so a turn that says nothing reports nothing instead
                    // of repeating the last turn's answer.
                    result: turn.answer.take(),
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
                    match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                        "tool_use" => {
                            if let Some(name) = s(b, "name") {
                                out.push(mk(Kind::Tool { name }));
                            }
                        }
                        // Held rather than published: while the turn is running
                        // this is a running answer, and only the last one before
                        // turn_duration is the answer. Clamped like every other
                        // result, so the log line stays atomically appendable.
                        "text" => {
                            if let Some(t) = s(b, "text") {
                                turn.answer = Some(one_line(&t, 160));
                            }
                        }
                        _ => {}
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

/// Where Claude keeps a subagent's sidecar, given the *parent's* transcript:
/// `<parent minus .jsonl>/subagents/agent-<id>.meta.json`.
pub fn subagent_meta_path(parent_transcript: &str, agent_id: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "{}/subagents/agent-{agent_id}.jsonl",
        parent_transcript.trim_end_matches(".jsonl")
    ))
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
    let mk = |result: Option<String>, tool: Option<String>| Event {
        ts: ts.clone(),
        source: "claude",
        session: parent_session.to_string(),
        kind: Kind::Subagent {
            id: agent_id.to_string(),
            state: "working",
            agent_type: Some(agent_type.to_string()),
            description: Some(description.to_string()),
            result,
            tool,
        },
    };
    let mut out = Vec::new();
    if v.get("type").and_then(|x| x.as_str()) == Some("assistant") {
        if let Some(blocks) = v.pointer("/message/content").and_then(|x| x.as_array()) {
            for b in blocks {
                match b.get("type").and_then(|x| x.as_str()) {
                    // The last assistant text is the running answer; when the
                    // file stops growing it is the final one. There is no
                    // end-of-subagent record to wait for.
                    Some("text") => {
                        if let Some(t) = s(b, "text") {
                            out.push(mk(Some(one_line(&t, 160)), None));
                        }
                    }
                    Some("tool_use") => {
                        if let Some(name) = s(b, "name") {
                            out.push(mk(None, Some(name)));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Even a record that says nothing new dates the subagent, which is the only
    // way its start time is known.
    if out.is_empty() {
        out.push(mk(None, None));
    }
    out
}
