//! Kimi Code adapter（`~/.kimi-code/sessions/wd_<目录哈希>/session_<id>/agents/<agent>/wire.jsonl`）。
//!
//! 格式（2026-08 实测解剖，wire 协议事件流）：
//! - 外层 `type` 只有少数承载内容，其余全是元数据（防御式跳过）：
//!   - `turn.prompt`：用户输入（权威来源）——`input[].text` → User 消息；
//!   - `context.append_loop_event`：agent 回合事件，`event.type` 分发：
//!     `content.part`（`part.type`: `text` → Assistant，`think` → Thinking）、
//!     `tool.call`（name+args → ToolCall，Write/Edit 提取 artifact）、
//!     `tool.result`（toolCallId+result.output → ToolResult）；
//!     `step.begin/end` 等元数据跳过；
//!   - `context.append_message`：turn.prompt 的上下文副本（实测同文重复），
//!     **整体跳过**——用户消息只认 turn.prompt，杜绝双录；
//!   - `llm.request` / `usage.record` / `token_counting.*` / `staleGuard.*` /
//!     `permission.*` / `mcp.*` / `config.*` / `tools.*` / `turn.ended` 等：
//!     纯元数据，跳过。
//! - 会话身份从路径派生：`session_<uuid>` 目录 + `agents/<名字>`——main 即
//!   本体（native_id = session_<uuid>），子 agent 加 `#<名字>` 后缀独立成会话。
//! - cwd 不在 wire.jsonl：enrich_meta_from_state 从同会话目录的 state.json
//!   只读补查（尽力而为）。
//!
//! parse 防御式：未知事件/损坏行跳过，永不 panic（AGENTS.md adapter 契约）。

use std::path::Path;

use serde_json::Value;

use crate::models::{MessageKind, NewArtifact, NewMessage, ParseOutput, SessionMetaPatch};

const MAX_CONTENT: usize = 200_000;

/// input/args 里 file_path 标记写文件产物的工具（与 claude/zcode 同口径）。
const FILE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

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
        let ts = v.get("time").and_then(Value::as_i64).and_then(ms_to_iso);
        if let Some(t) = ts.as_deref() {
            track_min(meta, t);
            track_max(meta, t);
        }
        match v.get("type").and_then(Value::as_str) {
            // 用户输入：turn.prompt 与 turn.steer（转向注入，同构）；
            // append_message 是它们的上下文副本，跳过防双录
            Some("turn.prompt") | Some("turn.steer") => {
                if let Some(items) = v.get("input").and_then(Value::as_array) {
                    for item in items {
                        if item.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(t) = item.get("text").and_then(Value::as_str) {
                                push(&mut out.messages, *line_no, MessageKind::User, t, ts.clone());
                            }
                        }
                    }
                }
            }
            Some("context.append_loop_event") => {
                let e = &v["event"];
                match e.get("type").and_then(Value::as_str) {
                    Some("content.part") => {
                        let part = &e["part"];
                        match part.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    push(&mut out.messages, *line_no, MessageKind::Assistant, t, ts.clone());
                                }
                            }
                            Some("think") => {
                                // 真实 wire 的推理内容在 part.think（codex 一审：
                                // 读 part.text 导致 1497 个 think 事件全丢）
                                if let Some(t) = part.get("think").and_then(Value::as_str) {
                                    push(&mut out.messages, *line_no, MessageKind::Thinking, t, ts.clone());
                                }
                            }
                            _ => {} // 未知 part 类型：跳过
                        }
                    }
                    Some("tool.call") => {
                        let name = e.get("name").and_then(Value::as_str).unwrap_or("?");
                        let args = e.get("args").cloned().unwrap_or(Value::Null);
                        let display = serde_json::to_string(&args).unwrap_or_default();
                        push(&mut out.messages, *line_no, MessageKind::ToolCall,
                            &format!("[{name}] {display}"), ts.clone());
                        if FILE_TOOLS.contains(&name) {
                            // 真实 Kimi 的写文件参数是 args.path（相对 cwd 的路径）
                            if let Some(fp) = args.get("path").and_then(Value::as_str) {
                                out.artifacts.push(NewArtifact { path: fp.to_string(), tool: name.to_string() });
                            }
                        }
                    }
                    Some("tool.result") => {
                        let id = e.get("toolCallId").and_then(Value::as_str).unwrap_or("?");
                        let value = e
                            .pointer("/result/output")
                            .map(|o| match o {
                                Value::String(s) => s.clone(),
                                other => serde_json::to_string(other).unwrap_or_default(),
                            })
                            .unwrap_or_default();
                        push(&mut out.messages, *line_no, MessageKind::ToolResult,
                            &format!("[{id}] {value}"), ts.clone());
                    }
                    _ => {} // step.begin/end 等元数据：跳过
                }
            }
            _ => {} // 其余外层类型（append_message/usage/llm.request/...）：跳过
        }
    }
    out
}

/// cwd 等元数据不在 wire.jsonl：从会话目录的 state.json 只读补查
/// （尽力而为，任何失败返回原 meta——解析不依赖它）。
pub fn enrich_meta_from_state(meta: &SessionMetaPatch, wire_path: &Path) -> SessionMetaPatch {
    let mut meta = meta.clone();
    // wire.jsonl 的上级结构：…/session_<id>/agents/<agent>/wire.jsonl
    let Some(session_dir) = wire_path.ancestors().nth(3) else {
        return meta;
    };
    let Ok(v) = std::fs::read_to_string(session_dir.join("state.json")) else {
        return meta;
    };
    if let Ok(j) = serde_json::from_str::<Value>(&v) {
        if let Some(cwd) = j.get("cwd").and_then(Value::as_str) {
            meta.cwd = Some(cwd.to_string());
        }
    }
    meta
}

/// wire 协议的 time 是 epoch 毫秒 → RFC3339（统一 schema 口径）。
/// 越界毫秒按缺失降级（None），不产生空字符串时间戳。
fn ms_to_iso(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn push(out: &mut Vec<NewMessage>, line_no: u64, kind: MessageKind, text: &str, ts: Option<String>) {
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
        timestamp: ts,
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
