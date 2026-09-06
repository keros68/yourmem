//! ZCode adapter（`~/.zcode/cli/rollout/model-io-sess_<id>.jsonl`）。
//!
//! 格式（2026-08 实测解剖）：每行一次模型 API 往返——
//! - 白名单：`type == "model_io"` 且 `querySource == "main_turn"` 才解析；
//!   `session_title` 与一切未知类型/来源跳过（防御式裁定：宁可漏一行，
//!   不错收一类）；
//! - 历史快照位置两代格式（0.3.8 真机：新版 185/185 行非空而 body 全空）：
//!   新版 `request.messages`，旧版 `request.body.messages`，取非空者；
//!
//! 提取规则（防重复是本 adapter 的头号问题）：
//! - assistant/toolcall 消息**只取每行 response**（权威来源）——request 历史
//!   里的 assistant 条目全部丢弃，否则一行历史就翻倍整段对话；
//! - user / role:tool(tool-result) / system 消息只能来自历史行。历史行是全量
//!   快照，去重键为 (kind+content 哈希, 快照内出现序号)：同一快照内两条同文
//!   消息都是真消息（保留）；下一个快照重复的是同一条（跳过）、新出现序号的
//!   才是新消息；
//! - 历史行还触发 `history_resync`：ingest 侧对整个文件全量重导（见 ingest.rs），
//!   保证跨增量 chunk 的重复历史最终一致——"重复 request 历史不产生重复消息"
//!   验收点的完整闭环；
//! - cwd 不在 rollout 里：ingest 侧用 enrich_meta_from_db 从
//!   `~/.zcode/cli/db/db.sqlite` 的 session.directory 只读查得（尽力而为）。
//!
//! parse 防御式：未知行/损坏行跳过，永不 panic（AGENTS.md adapter 契约）。

use std::collections::{HashMap, HashSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

use crate::models::{MessageKind, NewArtifact, NewMessage, ParseOutput, SessionMetaPatch};

const MAX_CONTENT: usize = 200_000;

/// input.file_path 标记写文件产物的工具（与 claude.rs 同口径）。
const FILE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// harness 上下文压缩的摘要前缀（0.3.8 真机验证：本会话压缩后 34 处命中——
/// 压缩之后每个携带全量历史的请求，其首条 user 消息都以它开头）。
/// 误报后果仅是压缩点标错位置，不丢数据；zcode 元库的 time_compacting 实测
/// 11 会话 0 填值，不可靠，故取本信号。
pub const COMPACT_SUMMARY_PREFIX: &str =
    "This session is being continued from a previous conversation";

pub fn parse_lines(lines: &[(u64, String)]) -> ParseOutput {
    let mut out = ParseOutput::default();
    let meta = &mut out.meta;
    // (kind+content 哈希, 快照内出现序号) 的已入账集合——历史快照间去重
    let mut seen: HashSet<(u64, u32)> = HashSet::new();

    for (line_no, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue, // partial / corrupt line: vault still has it
        };
        // 白名单：只认 model_io/main_turn，其余（session_title 与未来未知类型）跳过
        if v.get("type").and_then(Value::as_str) != Some("model_io") {
            continue;
        }
        if v.get("querySource").and_then(Value::as_str) != Some("main_turn") {
            continue;
        }
        let started = v.get("startedAt").and_then(Value::as_str);
        let completed = v.get("completedAt").and_then(Value::as_str);
        if let Some(t) = started {
            track_min(meta, t);
        }
        if let Some(t) = completed {
            track_max(meta, t);
        }
        let ts = started.or(completed).map(str::to_string);

        // 1) 本行产出：response.text（assistant）+ toolCalls
        if let Some(resp) = v.get("response") {
            if let Some(text) = resp.get("text").and_then(Value::as_str) {
                push(&mut out.messages, *line_no, MessageKind::Assistant, text, ts.clone());
            }
            if let Some(calls) = resp.get("toolCalls").and_then(Value::as_array) {
                for call in calls {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                    let input = call.get("input").cloned().unwrap_or(Value::Null);
                    let display = serde_json::to_string(&input).unwrap_or_default();
                    push(&mut out.messages, *line_no, MessageKind::ToolCall,
                        &format!("[{name}] {display}"), ts.clone());
                    if FILE_TOOLS.contains(&name) {
                        if let Some(fp) = input.get("file_path").and_then(Value::as_str) {
                            out.artifacts.push(NewArtifact { path: fp.to_string(), tool: name.to_string() });
                        }
                    }
                }
            }
        }

        // 2) 历史行（少数）：user / role:tool(tool-result) / system；assistant 全丢
        //    （response 已是权威来源，历史里的 assistant 只会是重复）。
        //    上下文位置两代格式（0.3.8 真机解剖）：新版在 request.messages
        //    （185/185 行非空，body.messages 全空），旧版在 request.body.messages；
        //    两者互斥，取非空者。
        let hist_msgs = v
            .pointer("/request/messages")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .or_else(|| v.pointer("/request/body/messages").and_then(Value::as_array))
            .filter(|a| !a.is_empty());
        if let Some(msgs) = hist_msgs {
            out.history_resync = true;
            // 压缩点检测（0.3.8）：任一 user 消息以 harness 摘要前缀开头，该行即
            // 压缩边界，更早的行是压缩前原文。摘要在消息序列中段（system/skill
            // 注入之后的第一条 user，真机 index 4），必须扫描不能只看首条；
            // chunk 内取最早，跨 chunk 由 ingest 对 sessions.compact_line_no 取 MIN。
            for m in msgs {
                if m.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                let head = match m.get("content") {
                    Some(Value::String(s)) => Some(s.as_str()),
                    Some(Value::Array(items)) => items.iter().find_map(|b| {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => b.get("text").and_then(Value::as_str),
                            _ => None,
                        }
                    }),
                    _ => None,
                };
                if head.is_some_and(|t| t.starts_with(COMPACT_SUMMARY_PREFIX)) {
                    out.compact_line =
                        Some(out.compact_line.map_or(*line_no, |l| l.min(*line_no)));
                    break;
                }
            }
            // 快照内出现序号：同快照同文的两条是两条真消息，跨快照同序号的才是重复
            let mut occ: HashMap<u64, u32> = HashMap::new();
            for m in msgs {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                match role {
                    "user" => extract_user_blocks(&m["content"], *line_no, &ts, &mut occ, &mut seen, &mut out.messages),
                    "tool" => extract_tool_blocks(&m["content"], *line_no, &ts, &mut occ, &mut seen, &mut out.messages),
                    "system" => {
                        if let Some(text) = m.get("content").and_then(Value::as_str) {
                            push_dedup(&mut out.messages, *line_no, MessageKind::System, text, ts.clone(), &mut occ, &mut seen);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// user content 可能是纯字符串或 block 数组（text/image 混排）。
fn extract_user_blocks(
    content: &Value,
    line_no: u64,
    ts: &Option<String>,
    occ: &mut HashMap<u64, u32>,
    seen: &mut HashSet<(u64, u32)>,
    out: &mut Vec<NewMessage>,
) {
    match content {
        Value::String(s) => push_dedup(out, line_no, MessageKind::User, s, ts.clone(), occ, seen),
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        push_dedup(out, line_no, MessageKind::User, t, ts.clone(), occ, seen);
                    }
                }
                // image / 未知块：跳过（防御式）
            }
        }
        _ => {}
    }
}

/// role:"tool" 的 content 两代形态（真机 2026-08-30）：旧版是 tool-result block
/// 数组（带 toolName）；新版是裸字符串或 text/image block 数组（4705/4720 为字符串）。
fn extract_tool_blocks(
    content: &Value,
    line_no: u64,
    ts: &Option<String>,
    occ: &mut HashMap<u64, u32>,
    seen: &mut HashSet<(u64, u32)>,
    out: &mut Vec<NewMessage>,
) {
    match content {
        Value::String(s) => {
            push_dedup(out, line_no, MessageKind::ToolResult, s, ts.clone(), occ, seen);
        }
        Value::Array(items) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("tool-result") => {
                        let name = item.get("toolName").and_then(Value::as_str).unwrap_or("?");
                        let value = item
                            .pointer("/output/value")
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                other => serde_json::to_string(other).unwrap_or_default(),
                            })
                            .unwrap_or_default();
                        push_dedup(out, line_no, MessageKind::ToolResult,
                            &format!("[{name}] {value}"), ts.clone(), occ, seen);
                    }
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            push_dedup(out, line_no, MessageKind::ToolResult, t, ts.clone(), occ, seen);
                        }
                    }
                    // image / 未知块：跳过（防御式）
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// 历史派生消息入账前按 (内容哈希, 快照内出现序号) 查重。
fn push_dedup(
    out: &mut Vec<NewMessage>,
    line_no: u64,
    kind: MessageKind,
    text: &str,
    ts: Option<String>,
    occ: &mut HashMap<u64, u32>,
    seen: &mut HashSet<(u64, u32)>,
) {
    let mut h = DefaultHasher::new();
    kind.as_str().hash(&mut h);
    text.hash(&mut h);
    let key = h.finish();
    let n = occ.entry(key).or_insert(0);
    *n += 1;
    if !seen.insert((key, *n)) {
        return; // 上一个快照里同序号的这条已经入账
    }
    push(out, line_no, kind, text, ts);
}

/// cwd 等元数据不在 rollout 文件里：从 zcode 自己的库里只读查
/// session.directory（尽力而为，任何失败都返回原 meta——文件解析不依赖它）。
pub fn enrich_meta_from_db(meta: &SessionMetaPatch, session_id: &str) -> SessionMetaPatch {
    let mut meta = meta.clone();
    let db = crate::home_dir().join(".zcode").join("cli").join("db").join("db.sqlite");
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return meta;
    };
    let native_id = session_id.rsplit(':').next().unwrap_or(session_id);
    let cwd: Option<String> = conn
        .query_row(
            "SELECT directory FROM session WHERE id = ?1",
            rusqlite::params![native_id],
            |r| r.get(0),
        )
        .ok();
    if cwd.is_some() {
        meta.cwd = cwd;
    }
    meta
}

fn push(out: &mut Vec<NewMessage>, line_no: u64, kind: MessageKind, text: &str, ts: Option<String>) {
    let content = truncate(text);
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

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_CONTENT {
        s.to_string()
    } else {
        format!("{}…[truncated]", s.chars().take(MAX_CONTENT).collect::<String>())
    }
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
