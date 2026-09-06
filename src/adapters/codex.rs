//! Codex adapter (`~/.codex/sessions/**/*.jsonl` rollout files).
//!
//! Canonical content comes from `response_item` payloads. `event_msg`
//! user/agent messages duplicate those in current-format files, so they are
//! only used when the chunk contains no response_item activity (older
//! formats). `token_count` events are skipped.

use serde_json::Value;

use crate::models::{MessageKind, NewArtifact, NewMessage, ParseOutput, SessionMetaPatch};

const MAX_CONTENT: usize = 200_000;

pub fn parse_lines(lines: &[(u64, String)], prior_saw_response_items: bool) -> ParseOutput {
    let mut output = ParseOutput::default();
    let meta = &mut output.meta;
    let out = &mut output.messages;
    let artifacts = &mut output.artifacts;

    // New-format marker: any response_item in this chunk (or in an earlier chunk
    // of the same file — prior_saw_response_items, persisted by ingest) means
    // event_msg user/agent_message lines are duplicates and must be suppressed.
    // 只看当前 chunk 会在增量边界漏判：response_item 行与它的 event_msg 副本
    // 恰好被两次导入切开时，副本会被当老格式再收一遍。
    let has_response_items =
        prior_saw_response_items || lines.iter().any(|(_, raw)| raw.contains("\"response_item\""));
    output.saw_response_items = has_response_items;

    for (line_no, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts = v.get("timestamp").and_then(Value::as_str).map(str::to_string);
        track_ts(meta, ts.as_deref());

        let line_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = &v["payload"];
        match line_type {
            "session_meta" | "turn_context" => {
                if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                    meta.cwd = Some(cwd.to_string());
                }
            }
            "response_item" => parse_response_item(payload, *line_no, &ts, out, artifacts),
            "event_msg" if !has_response_items => match payload.get("type").and_then(Value::as_str) {
                Some("user_message") => {
                    if let Some(t) = payload.get("message").and_then(Value::as_str) {
                        push(out, *line_no, MessageKind::User, t.to_string(), ts);
                    }
                }
                Some("agent_message") => {
                    if let Some(t) = payload.get("message").and_then(Value::as_str) {
                        push(out, *line_no, MessageKind::Assistant, t.to_string(), ts);
                    }
                }
                _ => {}
            },
            "compacted" => {
                let text = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("context compacted");
                push(out, *line_no, MessageKind::Summary, text.to_string(), ts);
            }
            _ => {}
        }
    }
    output
}

fn parse_response_item(
    payload: &Value,
    line_no: u64,
    ts: &Option<String>,
    out: &mut Vec<NewMessage>,
    artifacts: &mut Vec<NewArtifact>,
) {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            let kind = match role {
                "user" => MessageKind::User,
                "assistant" => MessageKind::Assistant,
                _ => MessageKind::System, // developer / system instructions
            };
            let text = content_text(&payload["content"]);
            if !text.is_empty() {
                push(out, line_no, kind, text, ts.clone());
            }
        }
        Some("reasoning") => {
            let text = payload["summary"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            if !text.is_empty() {
                push(out, line_no, MessageKind::Thinking, text, ts.clone());
            }
        }
        Some("function_call") | Some("custom_tool_call") | Some("local_shell_call") => {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .map(stringify)
                .unwrap_or_default();
            if name == "apply_patch" {
                for line in args.lines() {
                    if let Some(p) = line
                        .strip_prefix("*** Add File: ")
                        .or_else(|| line.strip_prefix("*** Update File: "))
                        .or_else(|| line.strip_prefix("*** Delete File: "))
                    {
                        artifacts.push(NewArtifact { path: p.trim().to_string(), tool: name.to_string() });
                    }
                }
            }
            push(out, line_no, MessageKind::ToolCall, format!("{name}: {args}"), ts.clone());
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            let text = payload.get("output").map(stringify).unwrap_or_default();
            push(out, line_no, MessageKind::ToolResult, text, ts.clone());
        }
        Some("web_search_call") => {
            let q = payload
                .get("action")
                .and_then(|a| a.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("");
            push(out, line_no, MessageKind::ToolCall, format!("web_search: {q}"), ts.clone());
        }
        _ => {}
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn push(out: &mut Vec<NewMessage>, line_no: u64, kind: MessageKind, content: String, ts: Option<String>) {
    let content = if content.chars().count() > MAX_CONTENT {
        let t: String = content.chars().take(MAX_CONTENT).collect();
        format!("{t}…[truncated]")
    } else {
        content
    };
    if content.trim().is_empty() {
        return;
    }
    let ord = out.iter().filter(|m| m.line_no == line_no).count() as u32;
    out.push(NewMessage { line_no, ord, kind, content, timestamp: ts, uuid: None });
}

fn track_ts(meta: &mut SessionMetaPatch, ts: Option<&str>) {
    let Some(ts) = ts else { return };
    if meta.started_at.as_deref().map(|s| s > ts).unwrap_or(true) {
        meta.started_at = Some(ts.to_string());
    }
    if meta.ended_at.as_deref().map(|s| s < ts).unwrap_or(true) {
        meta.ended_at = Some(ts.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &[&str]) -> Vec<(u64, String)> {
        raw.iter().enumerate().map(|(i, s)| ((i + 1) as u64, s.to_string())).collect()
    }

    #[test]
    fn parses_rollout_and_suppresses_event_msg_dupes() {
        let input = lines(&[
            r#"{"timestamp":"2026-03-09T14:01:13Z","type":"session_meta","payload":{"id":"abc","cwd":"/tmp/proj"}}"#,
            r#"{"timestamp":"2026-03-09T14:01:14Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"帮我看下特征筛选"}]}}"#,
            r#"{"timestamp":"2026-03-09T14:01:14Z","type":"event_msg","payload":{"type":"user_message","message":"帮我看下特征筛选"}}"#,
            r#"{"timestamp":"2026-03-09T14:01:15Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"先读文件"}]}}"#,
            r#"{"timestamp":"2026-03-09T14:01:16Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"*** Begin Patch\n*** Update File: src/main.rs\n@@\n*** End Patch"}}"#,
            r#"{"timestamp":"2026-03-09T14:01:17Z","type":"response_item","payload":{"type":"function_call_output","output":"a.rs"}}"#,
            r#"{"timestamp":"2026-03-09T14:01:18Z","type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
        ]);
        let out = parse_lines(&input, false);
        assert_eq!(out.meta.cwd.as_deref(), Some("/tmp/proj"));
        let kinds: Vec<_> = out.messages.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, vec![MessageKind::User, MessageKind::Thinking, MessageKind::ToolCall, MessageKind::ToolResult]);
        // event_msg duplicate must not appear
        assert_eq!(out.messages.iter().filter(|m| m.kind == MessageKind::User).count(), 1);
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].path, "src/main.rs");
    }

    #[test]
    fn falls_back_to_event_msg_for_old_format() {
        let input = lines(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"老格式消息"}}"#,
        ]);
        let out = parse_lines(&input, false);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].kind, MessageKind::User);
    }
}
