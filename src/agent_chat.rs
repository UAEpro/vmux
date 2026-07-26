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

fn parse_line(
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
