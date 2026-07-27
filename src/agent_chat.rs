//! Parse agent transcript files into structured chat items.
//!
//! Claude Code writes each conversation as JSONL under
//! `~/.claude/projects/<munged-cwd>/<session-uuid>.jsonl`; its hooks report
//! that path (`transcript_path`), which the daemon stores per pane. This
//! module turns a byte range of that file into `ChatItem`s: user bubbles,
//! assistant bubbles, and one-line "activity" entries for tool calls, so
//! clients (the phone app) can render a chat without scraping the TUI.
//!
//! Pure functions over bytes — no locks, no IO. Cursor semantics are byte
//! offsets: only complete lines (through the last `\n`) are consumed, so a
//! partially-written trailing line is re-read on the next poll and multi-byte
//! UTF-8 is never split.

use crate::protocol::ChatItem;
use serde_json::Value;
use std::collections::HashMap;

/// Cap on the tool/error detail shown in an activity line. Bubbles keep the
/// full text; only activity summaries truncate.
const ACTIVITY_DETAIL_MAX: usize = 120;

pub struct ParseOutcome {
    pub items: Vec<ChatItem>,
    /// Bytes consumed from the start of the chunk (ends on a `\n` boundary).
    pub consumed: u64,
}

/// Parse complete lines from `bytes`, a chunk of the transcript starting at
/// absolute byte offset `base` (used only to build stable fallback ids).
/// Malformed or irrelevant lines are skipped but still consumed.
pub fn parse_transcript_chunk(bytes: &[u8], base: u64) -> ParseOutcome {
    let consumed = match bytes.iter().rposition(|&b| b == b'\n') {
        Some(last_newline) => last_newline + 1,
        None => 0,
    };
    let mut items = Vec::new();
    // Best-effort map from tool_use id -> tool name, so errored tool_results
    // later in the same chunk can name the tool that failed.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut offset = base;
    for line in bytes[..consumed].split(|&b| b == b'\n') {
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&String::from_utf8_lossy(line)) else {
            continue;
        };
        parse_line(&value, line_offset, &mut tool_names, &mut items);
    }
    ParseOutcome {
        items,
        consumed: consumed as u64,
    }
}

/// Route a transcript line to the adapter for whatever agent wrote it.
///
/// The three formats are structurally disjoint, so the line itself says which
/// one it is — no need to know the agent up front, and a pane that switches
/// agents (or a path that lies) still parses correctly:
///
/// - Codex writes `{timestamp, type: "response_item", payload: {…}}`
/// - Claude Code nests the turn under `message: {role, content}`
/// - Grok Build puts `content` (and `tool_calls`) directly on the line
fn parse_line(
    value: &Value,
    line_offset: u64,
    tool_names: &mut HashMap<String, String>,
    items: &mut Vec<ChatItem>,
) {
    if value.get("payload").is_some() {
        parse_codex_line(value, line_offset, items);
        return;
    }
    if value.get("message").is_none() {
        parse_grok_line(value, line_offset, items);
        return;
    }
    parse_claude_line(value, line_offset, tool_names, items);
}

fn parse_claude_line(
    value: &Value,
    line_offset: u64,
    tool_names: &mut HashMap<String, String>,
    items: &mut Vec<ChatItem>,
) {
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return; // subagent traffic
    }
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(iso8601_to_unix_millis)
        .unwrap_or(0);
    let uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("off:{line_offset}"));
    let message = value.get("message");
    let content = message.and_then(|m| m.get("content"));
    // Every other line type (system, summary, mode, permission-mode,
    // file-history-*, attachment, ai-title, last-prompt, pr-link,
    // queue-operation, and whatever future versions add) falls through.
    match value.get("type").and_then(Value::as_str) {
        Some("user") => {
            if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
                return;
            }
            match content {
                Some(Value::String(text)) => {
                    let text = text.trim();
                    if text.starts_with("[Request interrupted") {
                        items.push(ChatItem {
                            kind: "activity".to_string(),
                            text: "interrupted".to_string(),
                            id: uuid,
                            ts,
                            tool: Some("interrupt".to_string()),
                            error: false,
                        });
                    } else if !text.is_empty() && !text.starts_with('<') {
                        // `<`-prefixed content is system-generated markup
                        // (<command-name>, <local-command-stdout>,
                        // <task-notification>, …), not something typed.
                        items.push(ChatItem {
                            kind: "user".to_string(),
                            text: text.to_string(),
                            id: uuid,
                            ts,
                            tool: None,
                            error: false,
                        });
                    }
                }
                Some(Value::Array(blocks)) => {
                    // Tool results come back as user-role lines. Successful
                    // ones are noise; failures are surfaced so unexpected
                    // in-turn errors are visible in chat.
                    for (index, block) in blocks.iter().enumerate() {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result")
                            || block.get("is_error").and_then(Value::as_bool) != Some(true)
                        {
                            continue;
                        }
                        let tool = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .and_then(|id| tool_names.get(id).cloned());
                        items.push(ChatItem {
                            kind: "activity".to_string(),
                            text: truncate_chars(&tool_result_text(block), ACTIVITY_DETAIL_MAX),
                            id: format!("{uuid}:{index}"),
                            ts,
                            tool,
                            error: true,
                        });
                    }
                }
                _ => {}
            }
        }
        Some("assistant") => {
            let Some(Value::Array(blocks)) = content else {
                return;
            };
            for (index, block) in blocks.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.trim().is_empty() {
                            items.push(ChatItem {
                                kind: "assistant".to_string(),
                                text: text.trim().to_string(),
                                id: format!("{uuid}:{index}"),
                                ts,
                                tool: None,
                                error: false,
                            });
                        }
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        if let Some(id) = block.get("id").and_then(Value::as_str) {
                            tool_names.insert(id.to_string(), name.clone());
                        }
                        items.push(ChatItem {
                            kind: "activity".to_string(),
                            text: truncate_chars(&tool_use_detail(block), ACTIVITY_DETAIL_MAX),
                            id: format!("{uuid}:{index}"),
                            ts,
                            tool: Some(name),
                            error: false,
                        });
                    }
                    // "thinking" (and future block types) are skipped. A
                    // config toggle rendering thinking as collapsed rows
                    // would slot in here.
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Codex rollout line: `{timestamp, type, payload}` under
/// `~/.codex/sessions/YYYY/MM/DD/rollout-<stamp>-<session>.jsonl`.
///
/// Only `response_item` carries the conversation. `event_msg`, `session_meta`,
/// `world_state`, `turn_context` and `compacted` are bookkeeping.
fn parse_codex_line(value: &Value, line_offset: u64, items: &mut Vec<ChatItem>) {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let Some(payload) = value.get("payload") else {
        return;
    };
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(iso8601_to_unix_millis)
        .unwrap_or(0);
    let id = payload
        .get("id")
        .or_else(|| payload.get("call_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("off:{line_offset}"));
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            // `developer` is the harness talking to the model, not the user.
            let kind = match payload.get("role").and_then(Value::as_str) {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => return,
            };
            let text = codex_message_text(payload);
            // Codex injects <environment_context>, <user_instructions> and
            // <permissions instructions> as user turns; they are not typed.
            if text.is_empty() || text.starts_with('<') {
                return;
            }
            items.push(ChatItem {
                kind: kind.to_string(),
                text,
                id,
                ts,
                tool: None,
                error: false,
            });
        }
        // `function_call` arguments are a JSON *string*; `custom_tool_call`
        // (apply_patch) carries a raw `input` string instead.
        Some("function_call") | Some("custom_tool_call") | Some("tool_search_call") => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let detail = match payload.get("input").and_then(Value::as_str) {
                Some(input) => first_line(input),
                None => match payload.get("arguments") {
                    Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
                        .ok()
                        .map(|args| codex_argument_detail(&args))
                        .unwrap_or_else(|| first_line(raw)),
                    Some(args) => codex_argument_detail(args),
                    None => String::new(),
                },
            };
            items.push(ChatItem {
                kind: "activity".to_string(),
                text: truncate_chars(&detail, ACTIVITY_DETAIL_MAX),
                id,
                ts,
                tool: Some(name),
                error: false,
            });
        }
        // reasoning (encrypted), *_output, and future types: nothing to show.
        _ => {}
    }
}

/// Join a Codex message's `input_text`/`output_text` blocks.
fn codex_message_text(payload: &Value) -> String {
    match payload.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        Some(Value::String(text)) => text.trim().to_string(),
        _ => String::new(),
    }
}

/// Pick the human-meaningful field out of a Codex tool call's arguments.
fn codex_argument_detail(args: &Value) -> String {
    for key in ["cmd", "command", "query", "path", "file_path", "pattern"] {
        match args.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => {
                return first_line(text);
            }
            // exec_command sometimes sends argv as an array.
            Some(Value::Array(parts)) => {
                let joined = parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                if !joined.trim().is_empty() {
                    return joined;
                }
            }
            _ => {}
        }
    }
    // Unknown tool shape (update_plan, write_stdin, MCP calls …): a compact
    // preview of the arguments still says more than a bare tool name.
    match args {
        Value::Object(fields) if !fields.is_empty() => first_line(&args.to_string()),
        Value::String(text) => first_line(text),
        _ => String::new(),
    }
}

/// Grok Build line from `~/.grok/sessions/<cwd>/<session>/chat_history.jsonl`.
///
/// Flat records with no timestamps: `content` sits on the line itself, and an
/// assistant turn carries its tool calls in `tool_calls`.
fn parse_grok_line(value: &Value, line_offset: u64, items: &mut Vec<ChatItem>) {
    let id = format!("off:{line_offset}");
    match value.get("type").and_then(Value::as_str) {
        Some("user") => {
            // `synthetic_reason` marks harness-injected context (<user_info>,
            // resumed-session preambles), not something the user typed.
            if value.get("synthetic_reason").is_some() {
                return;
            }
            let text = grok_text(value.get("content"));
            if text.is_empty() || text.starts_with('<') {
                return;
            }
            items.push(ChatItem {
                kind: "user".to_string(),
                text,
                id,
                ts: 0,
                tool: None,
                error: false,
            });
        }
        Some("assistant") => {
            let text = grok_text(value.get("content"));
            if !text.is_empty() {
                items.push(ChatItem {
                    kind: "assistant".to_string(),
                    text,
                    id: id.clone(),
                    ts: 0,
                    tool: None,
                    error: false,
                });
            }
            let Some(Value::Array(calls)) = value.get("tool_calls") else {
                return;
            };
            for (index, call) in calls.iter().enumerate() {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let detail = match call.get("arguments") {
                    Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
                        .ok()
                        .map(|args| codex_argument_detail(&args))
                        .unwrap_or_else(|| first_line(raw)),
                    Some(args) => codex_argument_detail(args),
                    None => String::new(),
                };
                items.push(ChatItem {
                    kind: "activity".to_string(),
                    text: truncate_chars(&detail, ACTIVITY_DETAIL_MAX),
                    id: format!("{id}:{index}"),
                    ts: 0,
                    tool: Some(name),
                    error: false,
                });
            }
        }
        // system prompt, encrypted reasoning, tool_result echoes and
        // backend_tool_call markers add nothing a reader wants.
        _ => {}
    }
}

/// Grok content is a plain string for assistants and a block list for users.
fn grok_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// First non-empty line, for tool inputs that are whole patches or scripts.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Best-effort one-line summary of a tool call's input for the activity row.
fn tool_use_detail(block: &Value) -> String {
    let Some(input) = block.get("input") else {
        return String::new();
    };
    for key in [
        "command",
        "file_path",
        "pattern",
        "description",
        "prompt",
        "url",
    ] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// Extract the error text from a `tool_result` block (string or text-block
/// array content).
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// Truncate to at most `max` chars on a char boundary (emoji-safe).
fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => format!("{}…", &text[..byte_index]),
        None => text.to_string(),
    }
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff]Z` into unix milliseconds. Transcript
/// timestamps are always UTC ("Z"); anything else returns None.
fn iso8601_to_unix_millis(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (time, millis) = match time.split_once('.') {
        Some((time, frac)) => {
            let frac: String = frac.chars().chain("000".chars()).take(3).collect();
            (time, frac.parse::<u64>().ok()?)
        }
        None => (time, 0),
    };
    let mut time_parts = time.split(':');
    let hour: u64 = time_parts.next()?.parse().ok()?;
    let minute: u64 = time_parts.next()?.parse().ok()?;
    let second: u64 = time_parts.next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = (year - era * 400) as u64;
    let month_adj = u64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * month_adj + 2) / 5 + u64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146097 + day_of_era as i64 - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &str) -> Vec<ChatItem> {
        parse_transcript_chunk(lines.as_bytes(), 0).items
    }

    #[test]
    fn user_prompt_becomes_bubble() {
        let items = parse(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-07-24T10:00:00.500Z","message":{"role":"user","content":"hello there"}}
"#,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "user");
        assert_eq!(items[0].text, "hello there");
        assert_eq!(items[0].id, "u1");
        assert_eq!(items[0].ts, 1_784_887_200_500);
    }

    #[test]
    fn noise_user_lines_are_skipped() {
        let items = parse(concat!(
            r#"{"type":"user","uuid":"u1","message":{"content":"<command-name>/clear</command-name>"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","message":{"content":"<task-notification>done</task-notification>"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u3","isMeta":true,"message":{"content":"meta note"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u4","isSidechain":true,"message":{"content":"subagent"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u5","message":{"content":"   "}}"#,
            "\n",
        ));
        assert!(items.is_empty(), "{items:?}");
    }

    #[test]
    fn interrupt_marker_becomes_activity() {
        let items = parse(
            r#"{"type":"user","uuid":"u1","message":{"content":"[Request interrupted by user]"}}
"#,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "activity");
        assert_eq!(items[0].tool.as_deref(), Some("interrupt"));
        assert!(!items[0].error);
    }

    #[test]
    fn assistant_blocks_split_into_bubble_and_activity() {
        let items = parse(
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-07-24T10:00:01Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"On it."},{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"cargo test"}}]}}
"#,
        );
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].kind, "assistant");
        assert_eq!(items[0].text, "On it.");
        assert_eq!(items[0].id, "a1:1");
        assert_eq!(items[1].kind, "activity");
        assert_eq!(items[1].tool.as_deref(), Some("Bash"));
        assert_eq!(items[1].text, "cargo test");
        assert_eq!(items[1].id, "a1:2");
    }

    #[test]
    fn successful_tool_results_skipped_errors_surfaced_with_tool_name() {
        let items = parse(concat!(
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"jq ."}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","is_error":true,"content":"Exit code 127\njq: command not found"}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"r2","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"ok fine"}]}}"#,
            "\n",
        ));
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].tool.as_deref(), Some("Bash"));
        assert!(!items[0].error);
        assert_eq!(items[1].kind, "activity");
        assert!(items[1].error);
        assert_eq!(items[1].tool.as_deref(), Some("Bash"));
        assert!(items[1].text.contains("Exit code 127"));
    }

    #[test]
    fn non_conversation_line_types_and_garbage_are_skipped() {
        let items = parse(concat!(
            r#"{"type":"mode","mode":"plan"}"#,
            "\n",
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            "\n",
            r#"{"type":"attachment"}"#,
            "\n",
            r#"{"type":"ai-title"}"#,
            "\n",
            "not json at all\n",
            r#"{"type":"user","uuid":"u1","message":{"content":"real"}}"#,
            "\n",
        ));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "real");
    }

    #[test]
    fn trailing_partial_line_is_not_consumed() {
        let full = r#"{"type":"user","uuid":"u1","message":{"content":"hi"}}"#;
        let partial = r#"{"type":"user","uuid":"u2","message":{"content":"inco"#;
        let bytes = format!("{full}\n{partial}");
        let outcome = parse_transcript_chunk(bytes.as_bytes(), 0);
        assert_eq!(outcome.items.len(), 1);
        assert_eq!(outcome.consumed, full.len() as u64 + 1);
        // Next poll re-reads from the cursor and picks up the completed line.
        let rest = format!("{partial}mplete\"}}}}\n");
        let outcome2 = parse_transcript_chunk(rest.as_bytes(), outcome.consumed);
        assert_eq!(outcome2.items.len(), 1);
        assert_eq!(outcome2.items[0].text, "incomplete");
    }

    #[test]
    fn emoji_round_trips_and_truncation_is_char_safe() {
        let long = "🎉".repeat(200);
        let line = format!(
            r#"{{"type":"assistant","uuid":"a1","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"{long}"}}}},{{"type":"text","text":"done 🎉"}}]}}}}"#
        );
        let items = parse(&format!("{line}\n"));
        assert_eq!(items.len(), 2);
        assert!(items[0].text.ends_with('…'));
        assert_eq!(items[0].text.chars().count(), ACTIVITY_DETAIL_MAX + 1);
        assert_eq!(items[1].text, "done 🎉");
    }

    #[test]
    fn missing_uuid_gets_offset_fallback_id() {
        let bytes = br#"{"type":"user","message":{"content":"anon"}}
"#;
        let outcome = parse_transcript_chunk(bytes, 500);
        assert_eq!(outcome.items[0].id, "off:500");
    }

    #[test]
    fn codex_rollout_lines_become_bubbles_and_activity() {
        // Shapes taken from a real ~/.codex/sessions/**/rollout-*.jsonl.
        let items = parse(concat!(
            r#"{"timestamp":"2026-07-26T02:03:01.000Z","type":"session_meta","payload":{"id":"x"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:02.000Z","type":"event_msg","payload":{"type":"agent_message"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:03.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"harness rules"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n cwd\n</environment_context>"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:05.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fork the repo"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:06.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs_1","encrypted_content":"…"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:07.000Z","type":"response_item","payload":{"type":"message","role":"assistant","id":"msg_1","content":[{"type":"output_text","text":"On it."}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:08.000Z","type":"response_item","payload":{"type":"function_call","id":"fc_1","name":"exec_command","arguments":"{\"cmd\":\"pwd && ls\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:09.000Z","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_1","name":"apply_patch","input":"*** Begin Patch\n*** Add File: a.ts\n+x"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-26T02:03:10.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"done"}}"#,
            "\n",
        ));
        let shape: Vec<_> = items
            .iter()
            .map(|i| (i.kind.as_str(), i.tool.as_deref(), i.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("user", None, "fork the repo"),
                ("assistant", None, "On it."),
                ("activity", Some("exec_command"), "pwd && ls"),
                ("activity", Some("apply_patch"), "*** Begin Patch"),
            ]
        );
        assert_eq!(items[0].ts, 1_785_031_385_000);
    }

    #[test]
    fn codex_tool_without_a_known_argument_key_still_shows_something() {
        let items = parse(
            r#"{"type":"response_item","payload":{"type":"function_call","id":"fc_9","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"audit\"}]}"}}
"#,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].tool.as_deref(), Some("update_plan"));
        assert!(items[0].text.contains("audit"), "{:?}", items[0].text);
    }

    #[test]
    fn grok_chat_history_lines_become_bubbles_and_activity() {
        // Shapes taken from a real ~/.grok/sessions/**/chat_history.jsonl.
        let items = parse(concat!(
            r#"{"type":"system","content":"You are Grok 4.5 …"}"#,
            "\n",
            r#"{"type":"user","synthetic_reason":"resumed","content":[{"type":"text","text":"<user_info>\nOS: linux\n</user_info>"}]}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"merge the PR"}]}"#,
            "\n",
            r#"{"type":"reasoning","id":"r1","encrypted_content":"…","summary":[]}"#,
            "\n",
            r#"{"type":"assistant","model_id":"grok-4.5","content":"Merging now.","tool_calls":[{"id":"call-1","name":"run_terminal_command","arguments":"{\"command\":\"gh pr merge 49\"}"}]}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"merged"}"#,
            "\n",
            r#"{"type":"backend_tool_call","kind":"search"}"#,
            "\n",
        ));
        let shape: Vec<_> = items
            .iter()
            .map(|i| (i.kind.as_str(), i.tool.as_deref(), i.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("user", None, "merge the PR"),
                ("assistant", None, "Merging now."),
                ("activity", Some("run_terminal_command"), "gh pr merge 49"),
            ]
        );
    }

    #[test]
    fn formats_are_told_apart_by_shape_not_by_path() {
        // One chunk containing all three dialects still parses each correctly:
        // panes can change agent, and a recorded path can be stale.
        let items = parse(concat!(
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"claude line"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"codex line"}]}}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"grok line"}]}"#,
            "\n",
        ));
        let texts: Vec<_> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["claude line", "codex line", "grok line"]);
    }

    #[test]
    fn timestamp_conversion_matches_known_values() {
        assert_eq!(iso8601_to_unix_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso8601_to_unix_millis("2026-07-22T06:12:49.185Z"),
            Some(1_784_700_769_185)
        );
        assert_eq!(iso8601_to_unix_millis("not a time"), None);
        assert_eq!(iso8601_to_unix_millis("2026-07-22T06:12:49"), None);
    }
}
