//! Claude Code adapter (`~/.claude/projects/**/*.jsonl`).
//!
//! Content-bearing line types: user, assistant, system, summary.
//! Compaction: older versions start a new file (summary line + compact
//! lineage edge); 2.1+ compacts in place (`system/compact_boundary` line
//! followed by an `isCompactSummary` user line) → `compact_line`.
//! Ignored: mode, permission-mode, attachment, queue-operation,
//! file-history-snapshot, last-prompt, and unknown future types.

use serde_json::Value;

use crate::models::{MessageKind, NewArtifact, NewMessage, ParseOutput, SessionMetaPatch, UuidSighting};



pub fn parse_lines(lines: &[(u64, String)]) -> ParseOutput {
    let mut out = ParseOutput::default();
    let meta = &mut out.meta;
    let mut saw_first_bearing = false;

    for (line_no, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue, // partial / corrupt line: vault still has it
        };
        if let Some(u) = v.get("uuid").and_then(Value::as_str) {
            out.uuids.push(UuidSighting { line_no: *line_no, uuid: u.to_string() });
        }
        let ts = v.get("timestamp").and_then(Value::as_str).map(str::to_string);
        track_ts(meta, ts.as_deref());
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            meta.cwd = Some(cwd.to_string());
        }
        if let Some(gb) = v.get("gitBranch").and_then(Value::as_str) {
            meta.git_branch = Some(gb.to_string());
        }

        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let uuid = v.get("uuid").and_then(Value::as_str).map(str::to_string);
        match kind {
            "user" | "assistant" => {
                // Lineage: parentUuid of the file's FIRST message-bearing
                // line. (Later lines all have parentUuid — it is the
                // in-file chain — so only the head of the file counts.)
                // 判据是"出现过 user/assistant 行"而非 messages 非空：首行内容
                // 为空（extract 后零消息）时，第二行的 parentUuid 属文件内链，
                // 当 fork 信号会误报（自检 A7）。
                if !saw_first_bearing {
                    saw_first_bearing = true;
                    meta.first_parent_uuid =
                        v.get("parentUuid").and_then(Value::as_str).map(str::to_string);
                }
                // 同文件压缩后紧跟的摘要行（isCompactSummary）是 harness 生成的
                // 压缩摘要，不是用户输入。
                let base = if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                    MessageKind::Summary
                } else if kind == "user" {
                    MessageKind::User
                } else {
                    MessageKind::Assistant
                };
                extract_blocks(&v["message"]["content"], base, *line_no, &ts, &uuid, &mut out.messages, &mut out.artifacts);
            }
            "system" => {
                // 同文件压缩（Claude Code 2.1 起）：compact_boundary 行即压缩点，
                // 之前的消息是压缩前原文；chunk 内取最早，跨 chunk 由 ingest 取 MIN。
                if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                    out.compact_line = Some(out.compact_line.map_or(*line_no, |c| c.min(*line_no)));
                }
                if let Some(text) = v.get("content").and_then(Value::as_str) {
                    push(&mut out.messages, *line_no, MessageKind::System, text.to_string(), ts, uuid);
                }
            }
            "summary" => {
                // A leading summary line marks a compact continuation.
                if let Some(leaf) = v.get("leafUuid").and_then(Value::as_str) {
                    meta.compact_leaf_uuid = Some(leaf.to_string());
                }
                if let Some(text) = v.get("summary").and_then(Value::as_str) {
                    push(&mut out.messages, *line_no, MessageKind::Summary, text.to_string(), ts, uuid);
                }
            }
            _ => {}
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn extract_blocks(
    content: &Value,
    base: MessageKind,
    line_no: u64,
    ts: &Option<String>,
    uuid: &Option<String>,
    out: &mut Vec<NewMessage>,
    artifacts: &mut Vec<NewArtifact>,
) {
    match content {
        Value::String(s) => push(out, line_no, base, s.clone(), ts.clone(), uuid.clone()),
        Value::Array(items) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            push(out, line_no, base, t.to_string(), ts.clone(), uuid.clone());
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = item.get("thinking").and_then(Value::as_str) {
                            push(out, line_no, MessageKind::Thinking, t.to_string(), ts.clone(), uuid.clone());
                        }
                    }
                    Some("tool_use") => {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = compact(&item["input"], 2000);
                        if super::FILE_TOOLS.contains(&name) {
                            if let Some(p) = item["input"].get("file_path").and_then(Value::as_str) {
                                artifacts.push(NewArtifact { path: p.to_string(), tool: name.to_string() });
                            }
                        }
                        push(out, line_no, MessageKind::ToolCall, format!("{name}: {input}"), ts.clone(), uuid.clone());
                    }
                    Some("tool_result") => {
                        let text = match &item["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            other => compact(other, 2000),
                        };
                        push(out, line_no, MessageKind::ToolResult, text, ts.clone(), uuid.clone());
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn push(out: &mut Vec<NewMessage>, line_no: u64, kind: MessageKind, content: String, ts: Option<String>, uuid: Option<String>) {
    super::push_message(out, line_no, kind, content, ts, uuid);
}

fn compact(v: &Value, max: usize) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => super::truncate_chars(s.clone(), max),
        other => super::truncate_chars(other.to_string(), max),
    }
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
    fn parses_user_string_content() {
        let input = lines(&[
            r#"{"type":"user","cwd":"/tmp/proj","gitBranch":"main","sessionId":"s1","uuid":"u1","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"内涝防治模型用 ln(WL+1)"}}"#,
        ]);
        let out = parse_lines(&input);
        assert_eq!(out.meta.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].kind, MessageKind::User);
        assert!(out.messages[0].content.contains("ln(WL+1)"));
        assert_eq!(out.uuids.len(), 1);
    }

    #[test]
    fn parses_assistant_blocks_and_skips_noise() {
        let input = lines(&[
            r#"{"type":"mode","mode":"normal","sessionId":"s1"}"#,
            r#"{"type":"attachment","attachment":{"type":"deferred_tools_delta"}}"#,
            r#"{"type":"assistant","uuid":"u2","timestamp":"2026-08-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"先想清楚"},{"type":"text","text":"好的"},{"type":"tool_use","name":"Write","input":{"file_path":"/tmp/a.rs","content":"x"}}]}}"#,
            r#"{"type":"user","uuid":"u3","timestamp":"2026-08-01T10:01:01Z","message":{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","text":"file1.rs"}]}]}}"#,
        ]);
        let out = parse_lines(&input);
        let kinds: Vec<_> = out.messages.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, vec![MessageKind::Thinking, MessageKind::Assistant, MessageKind::ToolCall, MessageKind::ToolResult]);
        assert!(out.messages[2].content.starts_with("Write:"));
        assert_eq!(out.messages[3].content, "file1.rs");
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].path, "/tmp/a.rs");
    }
}
