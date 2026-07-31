//! agy (Antigravity CLI, Gemini) transcript → normalised events.
//!
//! The third observable host, and the first whose turn boundary reaches no file
//! we can read. Everything else is a plain JSONL transcript with a monotonic
//! `step_index` and an ISO `created_at`, so tailing it needs nothing new:
//!
//!   ~/.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript.jsonl
//!
//! It is written for every conversation whether or not hooks are configured —
//! verified against conversations predating any hook here — so discovery does
//! not depend on the agent having been wired up. The `.db` files under
//! `conversations/` are the conversation store and are deliberately not read:
//! they are protobuf blobs, and nothing here needs them.
//!
//! What has to be reported instead of observed is the end of a turn. agy writes
//! no record for it, and it is not derivable: the obvious rule — a
//! `PLANNER_RESPONSE` carrying prose and no tool calls, which is agy's own
//! `NO_TOOL_CALL` termination reason — yields 13 candidates across a 7-turn
//! conversation, because the model narrates between tool batches. Closing a
//! turn on that would end it several times over. So the `Stop` hook carries the
//! boundary, exactly as Claude's `turn_duration` record does, and the answer is
//! read back out of this transcript at that moment.

use crate::event::{one_line, Event, Kind};
use serde_json::Value;
use std::path::Path;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(|x| x.as_str())
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
}

/// The conversation id, which is also the session id, taken from the path.
///
/// `…/brain/<id>/.system_generated/logs/transcript.jsonl` — the id is three
/// directories up. Deriving it from the path rather than the contents means a
/// session is named from its first line rather than only once it happens to
/// mention itself.
pub fn session_of(path: &Path) -> String {
    path.ancestors()
        .nth(3)
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The prompt out of a `USER_INPUT` record.
///
/// agy wraps it in `<USER_REQUEST>` and appends `<ADDITIONAL_METADATA>` and
/// sometimes `<USER_SETTINGS_CHANGE>` blocks — the local time, settings the user
/// changed — which are machinery rather than anything typed. Taking the whole
/// content would put a timestamp in every session's label.
fn user_request(content: &str) -> Option<String> {
    if let Some(rest) = content.split_once("<USER_REQUEST>") {
        let body = rest.1.split("</USER_REQUEST>").next().unwrap_or("").trim();
        if !body.is_empty() {
            return Some(body.to_string());
        }
        return None;
    }
    // No wrapper: take it, unless it opens as one of the XML-ish blocks above,
    // which is the same guard codex's replayed context blocks need.
    let t = content.trim();
    if t.is_empty() || t.starts_with('<') {
        None
    } else {
        Some(t.to_string())
    }
}

/// One transcript line.
pub fn normalize(v: &Value, session: &str) -> Vec<Event> {
    let ts = s(v, "created_at").unwrap_or_default();
    let mk = |kind: Kind| Event {
        ts: ts.clone(),
        source: "agy",
        session: session.to_string(),
        kind,
    };
    let mut out = Vec::new();

    // Tool calls ride on the model's own record, named as agy names them for
    // hook matchers. The RUN_COMMAND / VIEW_FILE / LIST_DIRECTORY records that
    // follow are those calls *returning* — counting both double-counts every
    // tool, the same trap codex's `*_output` records set.
    if let Some(calls) = v.get("tool_calls").and_then(|x| x.as_array()) {
        for c in calls {
            if let Some(name) = s(c, "name") {
                out.push(mk(Kind::Tool { name }));
            }
        }
        return out;
    }

    match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        "USER_INPUT" => {
            if let Some(t) = s(v, "content").as_deref().and_then(user_request) {
                out.push(mk(Kind::Prompt {
                    text: one_line(&t, 160),
                }));
            }
        }
        // Prose from the model. Published as a label rather than an answer:
        // during a turn this is narration between tool batches, and which one
        // turns out to be the answer is only known once the turn ends — which
        // this transcript never says.
        "PLANNER_RESPONSE" => {
            if let Some(t) = s(v, "content") {
                out.push(mk(Kind::Label {
                    text: one_line(&t, 160),
                }));
            }
        }
        _ => {}
    }
    out
}

/// The turn's answer, read back when the `Stop` hook says the turn is over.
///
/// The last `PLANNER_RESPONSE` carrying prose. Records whose text is a tool
/// call carry no `content` at all, so they cannot be mistaken for it.
///
/// Read at turn end rather than accumulated while tailing: the hook fires from
/// the agent's own process after the model has finished, so the record is
/// already on disk, and reading then keeps the answer off the hook's stdin —
/// where it would have to fit inside the inbox's atomic-append limit and would
/// be dropped, silently, for exactly the long answers worth having.
pub fn last_answer(transcript: &Path) -> (Option<String>, Option<String>) {
    let Ok(txt) = std::fs::read_to_string(transcript) else {
        return (None, None);
    };
    let mut last = None;
    for line in txt.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("PLANNER_RESPONSE") {
            continue;
        }
        if v.get("tool_calls").is_some() {
            continue;
        }
        if let Some(t) = s(&v, "content") {
            last = Some(t);
        }
    }
    match last {
        Some(t) => (Some(one_line(&t, 160)), Some(t)),
        None => (None, None),
    }
}

/// The transcript a hook payload names.
///
/// agy reports `transcriptPath` without its extension — the file on disk is
/// that path plus `.jsonl`. Both spellings are accepted so this keeps working
/// if that is ever tidied up.
pub fn transcript_path(reported: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(reported);
    if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
        return p;
    }
    std::path::PathBuf::from(format!("{reported}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_comes_from_the_brain_directory() {
        let p = Path::new(
            "/home/u/.gemini/antigravity-cli/brain/0179-abc/.system_generated/logs/transcript.jsonl",
        );
        assert_eq!(session_of(p), "0179-abc");
    }

    #[test]
    fn prompt_is_unwrapped_and_metadata_dropped() {
        let c = "<USER_REQUEST>\nfix the parser\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-07-28T14:54:43-04:00.\n</ADDITIONAL_METADATA>";
        assert_eq!(user_request(c).as_deref(), Some("fix the parser"));
    }

    #[test]
    fn bare_content_is_taken_but_xml_preamble_is_not() {
        assert_eq!(
            user_request("do the thing").as_deref(),
            Some("do the thing")
        );
        assert_eq!(user_request("<SOMETHING>x</SOMETHING>"), None);
        assert_eq!(user_request("   "), None);
    }

    /// The double-count trap: the call is on the model's record, and the
    /// execution record that follows must not be counted again.
    #[test]
    fn counts_the_call_not_its_result() {
        let call = json!({
            "type": "PLANNER_RESPONSE", "created_at": "2026-07-31T05:42:48Z",
            "tool_calls": [{"name": "run_command"}, {"name": "view_file"}]
        });
        let names: Vec<String> = normalize(&call, "s")
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Tool { name } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["run_command", "view_file"]);

        let result = json!({
            "type": "RUN_COMMAND", "created_at": "2026-07-31T05:42:49Z",
            "content": "ok", "exit_code": 0
        });
        assert!(normalize(&result, "s").is_empty());
    }

    #[test]
    fn extensionless_transcript_path_is_completed() {
        assert_eq!(
            transcript_path("/a/b/transcript"),
            Path::new("/a/b/transcript.jsonl")
        );
        assert_eq!(
            transcript_path("/a/b/transcript.jsonl"),
            Path::new("/a/b/transcript.jsonl")
        );
    }
}
