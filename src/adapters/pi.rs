//! pi adapter（`~/.pi/agent/sessions/--<cwd 转码>--/<时间戳>_<uuid>.jsonl`，
//! version 3 树状 JSONL；2026-09-10 真机样本 56 文件驱动，本文件 fixture 全合成）。
//!
//! 格式（真机实测词表）：
//! - 行级 `type` 只有 `session` / `message` / `model_change` / `thinking_level_change`：
//!   - `session`：头行（id/timestamp/cwd）——cwd 直接在文件里，无需补查；
//!   - `message`：唯一内容行，`message.role` 分流；
//!   - `model_change` / `thinking_level_change`：元数据，跳过。
//! - message 行的 role：
//!   - `user`：content[] 的 `text` 条目 → User（`image` 等未知条目跳过）；
//!   - `assistant`：content[] 分流——`thinking` → Thinking、`text` → Assistant、
//!     `toolCall` → ToolCall（`[name] args-json`；write/edit 按 arguments.path
//!     提取 artifact，与 kimi args.path 同口径）；
//!   - `toolResult`：独立 role（toolCallId/toolName/content[]）→ ToolResult，
//!     正文前缀用工具名不透明 call id——工具名可搜，id 不可搜；
//!   - 未知 role 跳过。
//! - 行内 `id`/`parentId` 是文件内 8-hex 分支树（重试语义），无跨会话谱系信号
//!   ——不记 uuid 目击、不报 first_parent_uuid。
//! - 时间戳原生 RFC3339（`…Z`），同源同形，字典序 min/max 即时间窗；窗口按
//!   全部 message 行计（含缺体/未知条目的未入账行——文件说有过活动就算活动）。
//! - 防御式：损坏行/未知类型/未知条目跳过，永不 panic；version 字段不拒收
//!   （格式漂移只会少采，不会错采）。


use serde_json::Value;

use crate::models::{MessageKind, NewArtifact, NewMessage, ParseOutput, SessionMetaPatch};

const MAX_CONTENT: usize = 200_000;

/// pi 的写文件工具（真机词表：bash/read/edit/write/grep/ls/find）。
const FILE_TOOLS: [&str; 2] = ["write", "edit"];

pub fn parse_lines(lines: &[(u64, String)]) -> ParseOutput {
    let mut out = ParseOutput::default();
    let meta = &mut out.meta;

    for (line_no, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue, // partial / corrupt line: vault still has it
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                    meta.cwd = Some(cwd.to_string());
                }
                if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
                    track_min(meta, ts);
                    track_max(meta, ts);
                }
            }
            Some("message") => {
                let Some(ts) = v.get("timestamp").and_then(Value::as_str).map(str::to_string) else {
                    continue;
                };
                track_min(meta, &ts);
                track_max(meta, &ts);
                let msg = &v["message"];
                let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
                let Some(items) = msg.get("content").and_then(Value::as_array) else {
                    continue;
                };
                match role {
                    "user" => {
                        for item in items {
                            if item.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(t) = item.get("text").and_then(Value::as_str) {
                                    push(&mut out.messages, *line_no, MessageKind::User, t, Some(&ts));
                                }
                            }
                        }
                    }
                    "assistant" => {
                        for item in items {
                            match item.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                                        push(&mut out.messages, *line_no, MessageKind::Assistant, t, Some(&ts));
                                    }
                                }
                                Some("thinking") => {
                                    if let Some(t) = item.get("thinking").and_then(Value::as_str) {
                                        push(&mut out.messages, *line_no, MessageKind::Thinking, t, Some(&ts));
                                    }
                                }
                                Some("toolCall") => {
                                    let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
                                    let args = item.get("arguments").cloned().unwrap_or(Value::Null);
                                    let display = serde_json::to_string(&args).unwrap_or_default();
                                    push(&mut out.messages, *line_no, MessageKind::ToolCall,
                                        &format!("[{name}] {display}"), Some(&ts));
                                    if FILE_TOOLS.contains(&name) {
                                        if let Some(fp) = args.get("path").and_then(Value::as_str) {
                                            out.artifacts.push(NewArtifact {
                                                path: fp.to_string(),
                                                tool: name.to_string(),
                                            });
                                        }
                                    }
                                }
                                _ => {} // image 等未知条目：跳过
                            }
                        }
                    }
                    "toolResult" => {
                        let name = msg.get("toolName").and_then(Value::as_str).unwrap_or("?");
                        let mut text = String::new();
                        for item in items {
                            if item.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(t) = item.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(t);
                                }
                            }
                        }
                        push(&mut out.messages, *line_no, MessageKind::ToolResult,
                            &format!("[{name}] {text}"), Some(&ts));
                    }
                    _ => {} // 未知 role：跳过
                }
            }
            _ => {} // model_change / thinking_level_change / 未来类型：跳过
        }
    }
    out
}

fn push(out: &mut Vec<NewMessage>, line_no: u64, kind: MessageKind, text: &str, ts: Option<&str>) {
    let content: String = if text.chars().count() > MAX_CONTENT {
        format!("{}…[truncated]", text.chars().take(MAX_CONTENT).collect::<String>())
    } else {
        text.to_string()
    };
    if content.trim().is_empty() {
        return;
    }
    out.push(NewMessage {
        line_no,
        ord: out.iter().filter(|m| m.line_no == line_no).count() as u32,
        kind,
        content,
        timestamp: ts.map(str::to_string),
        uuid: None,
    });
}

fn track_min(meta: &mut SessionMetaPatch, ts: &str) {
    if meta.started_at.as_deref().map_or(true, |t| ts < t) {
        meta.started_at = Some(ts.to_string());
    }
}

fn track_max(meta: &mut SessionMetaPatch, ts: &str) {
    if meta.ended_at.as_deref().map_or(true, |t| ts > t) {
        meta.ended_at = Some(ts.to_string());
    }
}
