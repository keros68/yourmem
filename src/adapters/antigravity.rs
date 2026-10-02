//! Antigravity CLI adapter（`~/.gemini/antigravity-cli/brain/<uuid>/.system_generated/
//! logs/transcript_full.jsonl`；2026-10-02 真机 55 个会话驱动，fixture 全合成）。
//!
//! 同目录还有 `transcript.jsonl`（长字段截断版，`truncated_fields` 标注）与
//! `chunks/transcript_full/*.jsonl`（full 文件的字节切片，拼接后与之逐字节相同）——
//! 只采 `transcript_full.jsonl`，见 `is_transcript`。IDE 主存储
//! （conversations/<id>.db 的 protobuf blob）不读。
//!
//! 行格式：每行一个 step，`type` 分流——
//! - `USER_INPUT`：正文包在 `<USER_REQUEST>…</USER_REQUEST>` 里，其后是 harness
//!   附加的元数据块，只取请求本体 → User；
//! - `PLANNER_RESPONSE`：`thinking` → Thinking、`content` → Assistant、
//!   `tool_calls[]` → ToolCall（`[name] args`）；args 的值是 JSON 编码过的字符串，
//!   解一层再展示；`write_to_file`/`replace_file_content` 的 TargetFile 记为产物；
//! - `GENERIC`（source=MODEL）：工具输出 → ToolResult；
//! - `SYSTEM_MESSAGE` / `ERROR_MESSAGE` → System；
//! - `CHECKPOINT`：同文件压缩摘要 → Summary，该行即压缩点；
//! - 其他类型跳过。时间取 `created_at`（RFC3339 UTC）。
//!
//! cwd：优先 `history.jsonl` 里该会话的 workspace（启动目录，ingest 侧补查），
//! 否则取首个 `run_command` 的 `Cwd` 参数。

use std::path::Path;

use serde_json::{Map, Value};

use crate::models::{MessageKind, NewArtifact, NewMessage, ParseOutput, SessionMetaPatch};

pub const AGENT_ANTIGRAVITY: &str = "antigravity";

const WRITE_TOOLS: [&str; 2] = ["write_to_file", "replace_file_content"];

/// 只认 `<brain>/<uuid>/.system_generated/logs/transcript_full.jsonl`。
pub fn is_transcript(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "transcript_full.jsonl")
        && path.parent().and_then(Path::file_name).is_some_and(|n| n == "logs")
}

/// 会话 id = brain 下的目录名。
pub fn native_id(path: &Path) -> Option<String> {
    path.ancestors().nth(3)?.file_name().map(|n| n.to_string_lossy().to_string())
}

pub fn parse_lines(lines: &[(u64, String)]) -> ParseOutput {
    let mut out = ParseOutput::default();
    for (line_no, raw) in lines {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue; // partial / corrupt line: vault still has it
        };
        let ts = v.get("created_at").and_then(Value::as_str).map(str::to_string);
        if let Some(t) = ts.as_deref() {
            track_ts(&mut out.meta, t);
        }
        let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("");
        let msgs = &mut out.messages;
        match v.get("type").and_then(Value::as_str) {
            Some("USER_INPUT") => push(msgs, *line_no, MessageKind::User, user_request(text("content")), &ts),
            Some("PLANNER_RESPONSE") => {
                push(msgs, *line_no, MessageKind::Thinking, text("thinking"), &ts);
                push(msgs, *line_no, MessageKind::Assistant, text("content"), &ts);
                for call in v.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                    let args = decode_args(call.get("args"));
                    let display = serde_json::to_string(&args).unwrap_or_default();
                    push(msgs, *line_no, MessageKind::ToolCall, &format!("[{name}] {display}"), &ts);
                    if WRITE_TOOLS.contains(&name) {
                        if let Some(fp) = args.get("TargetFile").and_then(Value::as_str) {
                            out.artifacts.push(NewArtifact { path: fp.to_string(), tool: name.to_string() });
                        }
                    }
                    if name == "run_command" && out.meta.cwd.is_none() {
                        out.meta.cwd = args.get("Cwd").and_then(Value::as_str).map(str::to_string);
                    }
                }
            }
            Some("GENERIC") => push(msgs, *line_no, MessageKind::ToolResult, text("content"), &ts),
            Some("SYSTEM_MESSAGE") => push(msgs, *line_no, MessageKind::System, text("content"), &ts),
            Some("ERROR_MESSAGE") => {
                let body = if text("content").is_empty() { text("error") } else { text("content") };
                push(msgs, *line_no, MessageKind::System, body, &ts);
            }
            Some("CHECKPOINT") => {
                push(msgs, *line_no, MessageKind::Summary, text("content"), &ts);
                out.compact_line = Some(out.compact_line.map_or(*line_no, |l| l.min(*line_no)));
            }
            _ => {}
        }
    }
    out
}

/// `<USER_REQUEST>` 包裹的请求本体；没有包裹时原样返回。
fn user_request(content: &str) -> &str {
    content
        .split_once("<USER_REQUEST>")
        .and_then(|(_, rest)| rest.split_once("</USER_REQUEST>"))
        .map_or(content, |(body, _)| body.trim())
}

/// args 的值是 JSON 编码过的字符串（`"\"D:/a.md\""`）：能解一层就解。
fn decode_args(args: Option<&Value>) -> Value {
    let Some(Value::Object(map)) = args else {
        return args.cloned().unwrap_or(Value::Null);
    };
    let decoded: Map<String, Value> = map
        .iter()
        .map(|(k, v)| {
            let d = v.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).unwrap_or_else(|| v.clone());
            (k.clone(), d)
        })
        .collect();
    Value::Object(decoded)
}

/// 会话启动目录：`<antigravity-cli>/history.jsonl` 里该会话的 workspace。
pub fn enrich_meta_from_history(meta: &SessionMetaPatch, transcript: &Path) -> SessionMetaPatch {
    let mut meta = meta.clone();
    let (Some(id), Some(cli_root)) = (native_id(transcript), transcript.ancestors().nth(5)) else {
        return meta;
    };
    let Ok(history) = std::fs::read_to_string(cli_root.join("history.jsonl")) else {
        return meta;
    };
    let workspace = history
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("conversationId").and_then(Value::as_str) == Some(id.as_str()))
        .and_then(|v| v.get("workspace").and_then(Value::as_str).map(str::to_string));
    if workspace.is_some() {
        meta.cwd = workspace;
    }
    meta
}

fn push(out: &mut Vec<NewMessage>, line_no: u64, kind: MessageKind, text: &str, ts: &Option<String>) {
    super::push_message(out, line_no, kind, text.to_string(), ts.clone(), None);
}

fn track_ts(meta: &mut SessionMetaPatch, ts: &str) {
    if meta.started_at.as_deref().map_or(true, |t| ts < t) {
        meta.started_at = Some(ts.to_string());
    }
    if meta.ended_at.as_deref().map_or(true, |t| ts > t) {
        meta.ended_at = Some(ts.to_string());
    }
}
