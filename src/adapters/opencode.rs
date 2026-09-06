//! OpenCode adapter (`~/.local/share/opencode/opencode.db`, SQLite).
//!
//! The source DB is opened strictly read-only and never written. Unlike the
//! JSONL agents, content lives in `message`/`part` tables; the ingest unit
//! is one part row (vault archives its raw JSON together with the parent
//! message's JSON), and incrementality uses a `time_created` millisecond
//! cursor per session instead of a byte offset.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::OptionalExtension as _;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::db;
use crate::models::{MessageKind, NewArtifact, NewMessage};
use crate::vault;

pub const AGENT_OPENCODE: &str = "opencode";

const MAX_CONTENT: usize = 200_000;

pub fn default_db_path() -> PathBuf {
    // Windows 布局未验证（无真实样本，拿到样本再改），用 YOUMEM_OPENCODE_DB 覆盖。
    crate::home_dir().join(".local").join("share").join("opencode").join("opencode.db")
}

#[derive(Default)]
pub struct OpencodeOutcome {
    pub sessions_seen: usize,
    pub sessions_updated: usize,
    pub messages_added: u64,
    pub lines_archived: u64,
}

struct OcSession {
    id: String,
    parent_id: Option<String>,
    directory: Option<String>,
    time_created: Option<i64>,
    time_updated: Option<i64>,
}

pub fn import(conn: &mut Connection, home: &Path, db_path: &Path) -> Result<OpencodeOutcome> {
    let mut out = OpencodeOutcome::default();
    if !db_path.is_file() {
        return Ok(out);
    }
    let src = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} read-only", db_path.display()))?;
    // 源库可能正被 opencode 写：读连接也要 busy_timeout，否则写入高峰期 SELECT
    // 直接 SQLITE_BUSY 连坐整次 import（自检 B6；目标库侧 db::open 已统一 5s）
    src.busy_timeout(std::time::Duration::from_secs(5))?;

    let sessions: Vec<OcSession> = {
        let mut stmt = src.prepare(
            "SELECT id, parent_id, directory, time_created, time_updated FROM session",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(OcSession {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                directory: r.get(2)?,
                time_created: r.get(3)?,
                time_updated: r.get(4)?,
            })
        })?;
        let collected: std::result::Result<Vec<_>, _> = rows.collect();
        collected?
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
    sess: &OcSession,
) -> Result<(u64, u64)> {
    let src_key = format!("opencode://{}", sess.id);
    // 墓碑（v6）：被物理清除的会话不再导入（opencode 的“源”是整库 SQLite，
    // 墓碑按 session_id 匹配——按 path 会误杀整个 opencode 库）
    {
        let session_key = format!("{AGENT_OPENCODE}:{}", sess.id);
        let purged: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM purged_sources WHERE session_id = ?1",
                rusqlite::params![session_key],
                |r| r.get(0),
            )
            .optional()?;
        if purged.is_some() {
            return Ok((0, 0));
        }
    }
    let state = db::source_file_state(conn, AGENT_OPENCODE, &src_key)?;
    // imported_bytes 借用为 time_created 毫秒游标；cursor_text 存同毫秒下
    // 最后一条 part 的 id 作为决胜值，避免同一毫秒写入的 part 被漏采。
    // 旧版库没有 cursor_text：回退到严格 > 游标（否则会重复采集游标处的 part）。
    let cursor = state.as_ref().map(|s| s.imported_bytes).unwrap_or(0) as i64;
    let cursor_id = state.as_ref().and_then(|s| s.cursor_text.clone());
    let mut line_no = state.as_ref().map(|s| s.line_count).unwrap_or(0);

    // New parts joined with their parent message data.
    let row = |r: &rusqlite::Row| -> rusqlite::Result<(i64, String, String, String)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    };
    let parts: Vec<(i64, String, String, String)> = match &cursor_id {
        Some(cid) => src
            .prepare(
                "SELECT p.time_created, p.id, p.data, m.data
                 FROM part p JOIN message m ON m.id = p.message_id
                 WHERE p.session_id = ?1
                   AND (p.time_created > ?2 OR (p.time_created = ?2 AND p.id > ?3))
                 ORDER BY p.time_created, p.id",
            )?
            .query_map(params![sess.id, cursor, cid], row)?
            .collect::<std::result::Result<Vec<_>, _>>()?,
        None => src
            .prepare(
                "SELECT p.time_created, p.id, p.data, m.data
                 FROM part p JOIN message m ON m.id = p.message_id
                 WHERE p.session_id = ?1 AND p.time_created > ?2
                 ORDER BY p.time_created, p.id",
            )?
            .query_map(params![sess.id, cursor], row)?
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };

    if parts.is_empty() {
        return Ok((0, 0));
    }
    // 查询按 (time_created, id) 升序，最后一行即新游标。
    let (max_ts, max_id) = (parts.last().unwrap().0, parts.last().unwrap().1.clone());

    let project_id = match sess.directory.as_deref() {
        Some(dir) => {
            let (root, name) = db::project_root_for(dir);
            Some(db::upsert_project(conn, &root, &name)?)
        }
        None => None,
    };

    let mut messages: Vec<NewMessage> = Vec::new();
    let mut artifacts: Vec<NewArtifact> = Vec::new();
    let mut units: Vec<(u64, Vec<u8>)> = Vec::new();

    for (part_ts, _part_id, part_data, msg_data) in &parts {
        line_no += 1;
        let part: Value = serde_json::from_str(part_data).unwrap_or(Value::Null);
        let msg: Value = serde_json::from_str(msg_data).unwrap_or(Value::Null);
        let iso = ms_to_iso(*part_ts);
        parse_part(&part, &msg, line_no, &iso, &mut messages, &mut artifacts);

        // Vault unit: raw part + parent message JSON, verbatim.
        let unit = serde_json::json!({
            "message": msg,
            "part": part,
            "time_created": part_ts,
        });
        units.push((line_no, serde_json::to_vec(&unit)?));
    }

    let native_id = &sess.id;
    let session_key = format!("{AGENT_OPENCODE}:{native_id}");
    let meta = crate::models::SessionMetaPatch {
        cwd: sess.directory.clone(),
        git_branch: None,
        started_at: sess.time_created.and_then(ms_to_iso),
        ended_at: sess.time_updated.and_then(ms_to_iso),
        first_parent_uuid: None,
        compact_leaf_uuid: None,
    };

    let tx = conn.transaction()?;
    db::upsert_session(&tx, &session_key, AGENT_OPENCODE, native_id, project_id, &src_key, &meta, messages.len() as u64)?;
    db::insert_messages(&tx, &session_key, &messages)?;
    db::insert_artifacts(&tx, &session_key, project_id, &artifacts)?;

    for (n, raw) in &units {
        let hash = vault::store_line(home, raw)?;
        tx.execute(
            "INSERT OR REPLACE INTO vault_lines(session_id, line_no, hash) VALUES (?1,?2,?3)",
            params![session_key, *n as i64, hash],
        )?;
    }

    // OpenCode records subagent sessions directly via parent_id.
    if let Some(parent) = &sess.parent_id {
        tx.execute(
            "INSERT OR IGNORE INTO session_links(child_session_id, parent_session_id, link_type, created_at)
             VALUES (?1,?2,'subagent',?3)",
            params![session_key, format!("{AGENT_OPENCODE}:{parent}"), crate::now_iso()],
        )?;
    }

    // message_count 插入后绝对重算（与 JSONL 路径同口径，自检 C7）：upsert 的
    // 累加项在 OR IGNORE 命中旧重复行时会虚增，重算以 messages 表为准
    tx.execute(
        "UPDATE sessions SET message_count = (SELECT COUNT(*) FROM messages WHERE session_id = ?1) WHERE id = ?1",
        params![session_key],
    )?;

    db::update_source_file(&tx, &src_key, AGENT_OPENCODE, max_ts as u64, line_no, Some(&max_id))?;
    tx.commit()?;
    Ok((messages.len() as u64, units.len() as u64))
}

fn parse_part(
    part: &Value,
    msg: &Value,
    line_no: u64,
    iso: &Option<String>,
    out: &mut Vec<NewMessage>,
    artifacts: &mut Vec<NewArtifact>,
) {
    let role = msg.pointer("/role").and_then(Value::as_str).unwrap_or("");
    let ts = iso.clone();
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            let kind = if role == "user" { MessageKind::User } else { MessageKind::Assistant };
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                push(out, line_no, kind, t.to_string(), ts);
            }
        }
        Some("reasoning") => {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                push(out, line_no, MessageKind::Thinking, t.to_string(), ts);
            }
        }
        Some("tool") => {
            let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let input = part.pointer("/state/input").map(compact).unwrap_or_default();
            push(out, line_no, MessageKind::ToolCall, format!("{tool}: {input}"), ts.clone());
            if let Some(file_path) = part
                .pointer("/state/input/filePath")
                .or_else(|| part.pointer("/state/input/file_path"))
                .and_then(Value::as_str)
            {
                if matches!(tool, "write" | "edit" | "multiedit" | "apply_patch" | "Write" | "Edit") {
                    artifacts.push(NewArtifact { path: file_path.to_string(), tool: tool.to_string() });
                }
            }
            if let Some(output) = part.pointer("/state/output").and_then(Value::as_str) {
                push(out, line_no, MessageKind::ToolResult, output.to_string(), ts);
            }
        }
        Some("patch") => {
            if let Some(files) = part.get("files").and_then(Value::as_array) {
                for f in files.iter().filter_map(Value::as_str) {
                    artifacts.push(NewArtifact { path: f.to_string(), tool: "patch".to_string() });
                }
            }
        }
        _ => {} // step-start / step-finish / snapshot / file / unknown
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

fn compact(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > 2000 {
        let t: String = s.chars().take(2000).collect();
        format!("{t}…[truncated]")
    } else {
        s
    }
}

pub fn ms_to_iso(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal OpenCode-shaped source DB and verify the import.
    #[test]
    fn imports_from_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("opencode.db");
        let home = tempfile::tempdir().unwrap();

        let src = Connection::open(&src_path).unwrap();
        src.execute_batch(
            "CREATE TABLE session (id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO session VALUES ('ses_a', NULL, '/tmp/oc-proj', 1000, 5000);
             INSERT INTO session VALUES ('ses_b', 'ses_a', '/tmp/oc-proj', 2000, 3000);
             INSERT INTO message VALUES ('msg_1', 'ses_a', 1000, '{\"role\":\"user\"}');
             INSERT INTO part VALUES ('prt_1', 'msg_1', 'ses_a', 1001, '{\"type\":\"text\",\"text\":\"内涝防治建模继续\"}');
             INSERT INTO part VALUES ('prt_2', 'msg_1', 'ses_a', 1002, '{\"type\":\"tool\",\"tool\":\"edit\",\"state\":{\"input\":{\"filePath\":\"/tmp/oc-proj/model.py\"},\"output\":\"ok\"}}');
             INSERT INTO part VALUES ('prt_3', 'msg_1', 'ses_b', 2001, '{\"type\":\"text\",\"text\":\"子代理结果\"}');",
        )
        .unwrap();
        drop(src);

        let mut conn = db::open(home.path()).unwrap();
        let outcome = import(&mut conn, home.path(), &src_path).unwrap();
        assert_eq!(outcome.sessions_seen, 2);
        assert_eq!(outcome.sessions_updated, 2);
        // text + tool_call + tool_result + subagent text = 4
        assert_eq!(outcome.messages_added, 4);

        // searchable
        let hits = db::search(&conn, &db::SearchOpts {
            query: "内涝".into(), project: None, agent: Some("opencode".into()), kind: None, limit: 10,
        })
        .unwrap();
        assert_eq!(hits.len(), 1);

        // artifact extracted
        let arts = db::list_artifacts(&conn, None, Some("opencode:ses_a"), 10).unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0]["path"], "/tmp/oc-proj/model.py");

        // subagent lineage recorded
        let lineage = db::lineage_for(&conn, "opencode:ses_b").unwrap();
        assert_eq!(lineage["parents"][0]["session_id"], "opencode:ses_a");
        assert_eq!(lineage["parents"][0]["type"], "subagent");

        // incremental: no new parts -> nothing changes
        let again = import(&mut conn, home.path(), &src_path).unwrap();
        assert_eq!(again.messages_added, 0);

        // append a new part -> picked up by the cursor
        let src = Connection::open(&src_path).unwrap();
        src.execute("INSERT INTO part VALUES ('prt_4', 'msg_1', 'ses_a', 1003, '{\"type\":\"text\",\"text\":\"GWR 完成\"}')", []).unwrap();
        drop(src);
        let third = import(&mut conn, home.path(), &src_path).unwrap();
        assert_eq!(third.messages_added, 1);

        // vault export reconstructs the units
        let out = home.path().join("oc-export.jsonl");
        let n = vault::export_session(&conn, home.path(), "opencode:ses_a", &out).unwrap();
        assert_eq!(n, 3);
        let rebuilt = std::fs::read_to_string(&out).unwrap();
        assert!(rebuilt.contains("GWR 完成"));

        // 同一毫秒的 part：靠 part id 决胜，游标不会漏采
        let src = Connection::open(&src_path).unwrap();
        src.execute("INSERT INTO part VALUES ('prt_5', 'msg_1', 'ses_a', 1003, '{\"type\":\"text\",\"text\":\"同毫秒补丁\"}')", []).unwrap();
        drop(src);
        let fourth = import(&mut conn, home.path(), &src_path).unwrap();
        assert_eq!(fourth.messages_added, 1);
    }
}
