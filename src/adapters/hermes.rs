//! Hermes adapter（`~/.hermes/state.db`，SQLite 单库源，opencode 模式）。
//!
//! 源库严格只读。hermes 是多端网关 agent（cli/feishu/telegram/tui/subagent/cron），
//! 会话表自带头衔/cwd/git/谱系/成本字段；消息按自增 id 排，游标 = 每会话已导入的
//! 最大 message id（存 source_files.imported_bytes）。
//!
//! 产品裁定：source='cron' 是运行日志不是知识资产，默认不采；
//! subagent/feishu/telegram/tui/cli 照采。
//!
//! 压缩（hermes 机制，2026-08-30 尚无真实样本——messages.compacted 全 0）：
//! hermes 压缩是同会话 in-place——原消息翻 compacted=1 软归档、摘要行新增插入。
//! 摘要行按 hermes 自述带特征前缀（COMPACT_SUMMARY_PREFIXES）；由于 message id
//! 即时间序，"id < 摘要行 id"恰好= 压缩发生前存在的全部原文（头/被摘要中段/
//! 保留尾），与 sessions.compact_line_no 的切片语义吻合。首例真实压缩触发后校准。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::OptionalExtension as _;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::db;
use crate::models::{MessageKind, NewArtifact, NewMessage, SessionMetaPatch};
use crate::vault;

pub const AGENT_HERMES: &str = "hermes";

const MAX_CONTENT: usize = 200_000;

/// hermes 压缩摘要行的内容特征前缀（按其自述摘要形态内置；
/// 真机尚无样本，首例触发后校准）。
pub const COMPACT_SUMMARY_PREFIXES: [&str; 3] = [
    "[CONTEXT COMPACTION — REFERENCE ONLY]",
    "[CONTEXT SUMMARY]:",
    "--- END OF CONTEXT SUMMARY",
];

/// 默认不采的来源（cron = 运行日志，不是知识资产）。
const EXCLUDED_SOURCES: [&str; 1] = ["cron"];

pub fn default_db_path() -> PathBuf {
    crate::home_dir().join(".hermes").join("state.db")
}

#[derive(Default)]
pub struct HermesOutcome {
    pub sessions_seen: usize,
    pub sessions_updated: usize,
    pub messages_added: u64,
    pub lines_archived: u64,
}

struct HSession {
    id: String,
    parent_id: Option<String>,
    source: Option<String>,
    cwd: Option<String>,
    git_repo_root: Option<String>,
    git_branch: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
}

pub fn import(conn: &mut Connection, home: &Path, db_path: &Path) -> Result<HermesOutcome> {
    let mut out = HermesOutcome::default();
    if !db_path.is_file() {
        return Ok(out);
    }
    let src = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} read-only", db_path.display()))?;
    src.busy_timeout(std::time::Duration::from_secs(5))?;

    let sessions: Vec<HSession> = {
        // EXCLUDED_SOURCES 是编译期常量，拼接无注入面；单一事实来源，
        // 避免 SQL 里再硬编码一份 'cron'（const 曾经悬空成 dead code）
        let excluded = EXCLUDED_SOURCES
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = src.prepare(&format!(
            "SELECT id, parent_session_id, source, cwd, git_repo_root, git_branch,
                    started_at, ended_at
             FROM sessions
             WHERE (archived IS NULL OR archived = 0)
               AND (source IS NULL OR source NOT IN ({excluded}))"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(HSession {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                source: r.get(2)?,
                cwd: r.get(3)?,
                git_repo_root: r.get(4)?,
                git_branch: r.get(5)?,
                started_at: r.get(6)?,
                ended_at: r.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    for sess in &sessions {
        out.sessions_seen += 1;
        let (msgs, lines) = import_session(conn, home, &src, sess)?;
        if lines > 0 {
            out.sessions_updated += 1;
            out.messages_added += msgs;
            out.lines_archived += lines;
        }
    }
    Ok(out)
}

fn import_session(
    conn: &mut Connection,
    home: &Path,
    src: &Connection,
    sess: &HSession,
) -> Result<(u64, u64)> {
    let src_key = format!("hermes://{}", sess.id);
    // 墓碑：同 opencode——源是整库，按 session_id 匹配，不按 path
    {
        let session_key = format!("{AGENT_HERMES}:{}", sess.id);
        let purged: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM purged_sources WHERE session_id = ?1",
                params![session_key],
                |r| r.get(0),
            )
            .optional()?;
        if purged.is_some() {
            return Ok((0, 0));
        }
    }
    let state = db::source_file_state(conn, AGENT_HERMES, &src_key)?;
    // imported_bytes 借用为"已导入的最大 message id"（id 全库自增，单调可靠）
    let cursor = state.as_ref().map(|s| s.imported_bytes).unwrap_or(0) as i64;
    let mut count = state.as_ref().map(|s| s.line_count).unwrap_or(0);

    let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>, Option<f64>, Option<String>, Option<i64>, Option<i64>)> = src
        .prepare(
            "SELECT id, role, content, tool_call_id, tool_name, timestamp, tool_calls, active, compacted
             FROM messages WHERE session_id = ?1 AND id > ?2 ORDER BY id",
        )?
        .query_map(params![sess.id, cursor], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Ok((0, 0));
    }

    // cwd 缺失（feishu/telegram 聊天不绑项目）时回退 git_repo_root；再缺则
    // project_id = NULL（进全局记忆桶，不进任何项目卷宗）
    let dir = sess.cwd.clone().or_else(|| sess.git_repo_root.clone());
    let project_id = match dir.as_deref() {
        Some(d) => {
            let (root, name) = db::project_root_for(d);
            Some(db::upsert_project(conn, &root, &name)?)
        }
        None => None,
    };

    let session_key = format!("{AGENT_HERMES}:{}", sess.id);
    let mut messages: Vec<NewMessage> = Vec::new();
    let mut artifacts: Vec<NewArtifact> = Vec::new();
    let mut compact_line: Option<i64> = None;
    let mut units: Vec<(u64, Vec<u8>)> = Vec::new();

    for (id, role, content, _tool_call_id, tool_name, ts, tool_calls, active, compacted) in &rows {
        let line_no = *id as u64;
        let iso = secs_to_iso(*ts);
        parse_message(role, content.as_deref(), tool_name.as_deref(), tool_calls.as_deref(), line_no, &iso, &mut messages, &mut artifacts);
        // 压缩点：摘要行特征前缀（机制性检测，无真实样本；见模块注释）
        if let Some(c) = content {
            if COMPACT_SUMMARY_PREFIXES.iter().any(|p| c.starts_with(p)) {
                compact_line = Some(compact_line.map_or(*id, |m| m.min(*id)));
            }
        }
        // Vault unit：整行关键字段 JSON 保真（含 active/compacted 软归档标志）
        let unit = serde_json::json!({
            "id": id, "session_id": sess.id, "role": role, "content": content,
            "tool_name": tool_name, "timestamp": ts, "tool_calls": tool_calls,
            "active": active, "compacted": compacted,
        });
        units.push((line_no, serde_json::to_vec(&unit)?));
    }

    let meta = SessionMetaPatch {
        cwd: dir,
        git_branch: sess.git_branch.clone(),
        started_at: secs_to_iso(sess.started_at),
        ended_at: secs_to_iso(sess.ended_at),
        first_parent_uuid: None,
        compact_leaf_uuid: None,
    };

    let tx = conn.transaction()?;
    db::upsert_session(&tx, &session_key, AGENT_HERMES, &sess.id, project_id, &src_key, &meta, messages.len() as u64)?;
    db::insert_messages(&tx, &session_key, &messages)?;
    db::insert_artifacts(&tx, &session_key, project_id, &artifacts)?;

    for (n, raw) in &units {
        let hash = vault::store_line(home, raw)?;
        tx.execute(
            "INSERT OR REPLACE INTO vault_lines(session_id, line_no, hash) VALUES (?1,?2,?3)",
            params![session_key, *n as i64, hash],
        )?;
    }

    // 谱系：subagent 显式标 subagent，其余 parent_session_id 记 continuation
    if let Some(parent) = &sess.parent_id {
        let link_type = if sess.source.as_deref() == Some("subagent") { "subagent" } else { "continuation" };
        tx.execute(
            "INSERT OR IGNORE INTO session_links(child_session_id, parent_session_id, link_type, created_at)
             VALUES (?1,?2,?3,?4)",
            params![session_key, format!("{AGENT_HERMES}:{parent}"), link_type, crate::now_iso()],
        )?;
    }
    // 压缩点取 MIN（跨轮导入幂等；多段压缩保持首次边界）
    if let Some(cl) = compact_line {
        tx.execute(
            "UPDATE sessions SET compact_line_no = ?2 WHERE id = ?1 AND (compact_line_no IS NULL OR compact_line_no > ?2)",
            params![session_key, cl],
        )?;
    }

    tx.execute(
        "UPDATE sessions SET message_count = (SELECT COUNT(*) FROM messages WHERE session_id = ?1) WHERE id = ?1",
        params![session_key],
    )?;

    let max_id = rows.last().map(|r| r.0).unwrap_or(cursor);
    count += rows.len() as u64;
    db::update_source_file(&tx, &src_key, AGENT_HERMES, max_id as u64, count, None)?;
    tx.commit()?;
    Ok((messages.len() as u64, units.len() as u64))
}

fn parse_message(
    role: &str,
    content: Option<&str>,
    tool_name: Option<&str>,
    tool_calls: Option<&str>,
    line_no: u64,
    ts: &Option<String>,
    out: &mut Vec<NewMessage>,
    artifacts: &mut Vec<NewArtifact>,
) {
    match role {
        "user" | "assistant" => {
            let kind = if role == "user" { MessageKind::User } else { MessageKind::Assistant };
            if let Some(c) = content {
                push(out, line_no, kind, c.to_string(), ts.clone());
            }
            // assistant 的 tool_calls 是 JSON 数组字符串（OpenAI 风格）
            if role == "assistant" {
                if let Some(tc) = tool_calls.and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                    if let Some(calls) = tc.as_array() {
                        for call in calls {
                            let name = call
                                .pointer("/function/name")
                                .and_then(Value::as_str)
                                .unwrap_or("?");
                            // arguments 可能是 JSON 字符串（OpenAI 风格）或已是对象
                            let args_val = call.pointer("/function/arguments").cloned();
                            let args_obj = args_val.as_ref().and_then(|v| match v {
                                Value::String(s) => serde_json::from_str::<Value>(s).ok(),
                                other => Some(other.clone()),
                            });
                            let args = args_val
                                .map(|v| match v {
                                    Value::String(s) => s,
                                    other => other.to_string(),
                                })
                                .unwrap_or_default();
                            push(out, line_no, MessageKind::ToolCall, format!("[{name}] {args}"), ts.clone());
                            if matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
                                if let Some(fp) = args_obj
                                    .as_ref()
                                    .and_then(|a| a.get("file_path"))
                                    .and_then(Value::as_str)
                                {
                                    artifacts.push(NewArtifact { path: fp.to_string(), tool: name.to_string() });
                                }
                            }
                        }
                    }
                }
            }
        }
        "tool" => {
            if let Some(c) = content {
                let body = match tool_name {
                    Some(name) => format!("[{name}] {c}"),
                    None => c.to_string(),
                };
                push(out, line_no, MessageKind::ToolResult, body, ts.clone());
            }
        }
        "session_meta" => {
            if let Some(c) = content {
                push(out, line_no, MessageKind::System, c.to_string(), ts.clone());
            }
        }
        _ => {} // 未知 role：防御式跳过
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

fn secs_to_iso(secs: Option<f64>) -> Option<String> {
    let ms = (secs? * 1000.0) as i64;
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
