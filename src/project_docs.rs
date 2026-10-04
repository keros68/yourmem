//! 项目进度文档跟踪：项目根目录下的指令与进度文档（CLAUDE.md、AGENTS.md、
//! PROGRESS.md、HANDOFF.md、MEMORY.md，加用户在 config.json `tracked_docs`
//! 登记的文件）。备份沿用 memfiles 的整文件快照与修订历史；本模块负责
//! 候选文件发现、"哪些对话写过它"（session_artifacts 路径对照）与过时提示。
//! 过时只做提示，不改文档、不改任何状态。

use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::{db, ingest};

/// 进度文档的 memory_files.agent 值：不属于任何 agent，停用 agent 不影响它。
pub const DOC_AGENT: &str = "doc";

/// 文档最后更新后，同项目又开了这么多段对话即提示可能过时。
pub const STALE_SESSIONS: i64 = 5;
/// 文档超过这么多天未更新、且期间有对话，即提示可能过时。
pub const STALE_DAYS: i64 = 14;

/// 单文件备份上限（所有监控文件）；超过这个量级通常已不是给 agent 读的文档。
pub const MAX_DOC_BYTES: u64 = 1 << 20;

/// 项目根目录下按文件名（不区分大小写）自动识别的进度文档。
const PROGRESS_NAMES: [&str; 4] = ["progress.md", "handoff.md", "handsoff.md", "memory.md"];

/// 一个项目根目录下应跟踪的文件：(agent, 绝对路径, 是否手动登记)，按路径键去重
/// （手动登记了自动识别的文件时保留自动那条）。CLAUDE.md / AGENTS.md 沿用原有
/// agent 归属（claude / codex），其余记为 doc。`tracked` 由调用方读一次传入。
pub fn root_candidates(tracked: &[(String, String)], root: &str) -> Vec<(String, PathBuf, bool)> {
    let root_path = Path::new(root);
    let mut out = Vec::new();
    for (agent, name) in [(crate::adapters::AGENT_CLAUDE, "CLAUDE.md"), (crate::adapters::AGENT_CODEX, "AGENTS.md")] {
        let p = root_path.join(name);
        if p.is_file() && inside_root(root_path, &p) {
            out.push((agent.to_string(), p, false));
        }
    }
    if let Ok(entries) = std::fs::read_dir(root_path) {
        let mut found: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                PROGRESS_NAMES.contains(&name.as_str())
            })
            .map(|e| e.path())
            .filter(|p| p.is_file() && inside_root(root_path, p))
            .collect();
        found.sort();
        out.extend(found.into_iter().map(|p| (DOC_AGENT.to_string(), p, false)));
    }
    let key = db::path_key(root);
    for (project, rel) in tracked {
        // config.json 可被手改：读取侧同样只接受项目内的相对路径
        if db::path_key(project) != key || !is_plain_rel(rel) {
            continue;
        }
        let p = root_path.join(rel);
        if inside_root(root_path, &p) {
            out.push((DOC_AGENT.to_string(), p, true));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|(_, p, _)| seen.insert(db::path_key(&p.to_string_lossy())));
    out
}

/// 只由普通路径段组成（无 ..、盘符、根）。
fn is_plain_rel(rel: &str) -> bool {
    !rel.is_empty() && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))
}

/// 文件（解析符号链接后）仍位于项目根目录内。文件不存在时视为在内——
/// 无内容可读，状态里如实显示"文件不存在"。
pub fn inside_root(root: &Path, path: &Path) -> bool {
    match (std::fs::canonicalize(root), std::fs::canonicalize(path)) {
        (Ok(r), Ok(t)) => t.starts_with(r),
        (_, Err(_)) => true,
        (Err(_), Ok(_)) => false,
    }
}

/// config.json `tracked_docs`：[{project: 项目路径, path: 相对路径}]。缺失/损坏按空处理。
pub fn tracked(home: &Path) -> Vec<(String, String)> {
    ingest::read_config(home)["tracked_docs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| Some((v["project"].as_str()?.to_string(), v["path"].as_str()?.to_string())))
        .collect()
}

/// 把用户输入（相对路径，或项目内的绝对路径）规整为以 / 分隔的相对路径；
/// 拒绝越出项目根目录的路径。
fn normalize_rel(root: &str, input: &str) -> Result<String> {
    // 去掉尾部分隔符：下面按 path_key 长度回切原串，二者长度须一致
    let input = input.trim().trim_end_matches(['/', '\\']);
    anyhow::ensure!(!input.is_empty(), "请填写文件路径");
    let p = Path::new(input);
    let rel = if p.is_absolute() {
        let (pk, rk) = (db::path_key(input), db::path_key(root));
        let sep = if cfg!(windows) { '\\' } else { '/' };
        let rest = pk.strip_prefix(&rk).filter(|r| r.starts_with(sep))
            .ok_or_else(|| anyhow::anyhow!("文件不在项目目录内：{input}"))?;
        // 取原输入的同长度尾段，保留原有大小写
        input[input.len() - rest.len() + 1..].to_string()
    } else {
        input.to_string()
    };
    let rel = rel.replace('\\', "/");
    anyhow::ensure!(is_plain_rel(&rel), "路径不能包含 .. 或盘符：{input}");
    Ok(rel)
}

fn project_path(conn: &Connection, project_id: i64) -> Result<String> {
    Ok(conn.query_row("SELECT path FROM projects WHERE id = ?1", params![project_id], |r| r.get(0))?)
}

/// 登记项比对：项目与相对路径都按路径键（Windows 不区分大小写与分隔符）。
fn same_entry(v: &Value, root: &str, rel: &str) -> bool {
    v["project"].as_str().map(db::path_key) == Some(db::path_key(root))
        && v["path"].as_str().map(db::path_key) == Some(db::path_key(rel))
}

pub fn track(conn: &Connection, home: &Path, project_id: i64, input: &str) -> Result<Value> {
    let root = project_path(conn, project_id)?;
    let rel = normalize_rel(&root, input)?;
    let abs = Path::new(&root).join(&rel);
    let meta = std::fs::metadata(&abs).map_err(|_| anyhow::anyhow!("文件不存在：{}", abs.display()))?;
    anyhow::ensure!(meta.is_file(), "不是文件：{}", abs.display());
    anyhow::ensure!(meta.len() <= MAX_DOC_BYTES, "文件超过 1 MiB，不适合作为进度文档跟踪");
    anyhow::ensure!(inside_root(Path::new(&root), &abs), "文件链接到项目目录之外：{rel}");
    // 已被自动识别的文件不重复登记
    let abs_key = db::path_key(&abs.to_string_lossy());
    let auto = root_candidates(&[], &root).iter().any(|(_, p, _)| db::path_key(&p.to_string_lossy()) == abs_key);
    let exists = auto || ingest::update_config(home, |cfg| {
        let mut list = cfg["tracked_docs"].as_array().cloned().unwrap_or_default();
        if list.iter().any(|v| same_entry(v, &root, &rel)) {
            return Ok(true);
        }
        list.push(json!({ "project": root, "path": rel }));
        cfg["tracked_docs"] = Value::Array(list);
        Ok(false)
    })?;
    Ok(json!({ "ok": true, "project": root, "path": rel, "already_tracked": exists }))
}

pub fn untrack(conn: &Connection, home: &Path, project_id: i64, input: &str) -> Result<Value> {
    let root = project_path(conn, project_id)?;
    let rel = normalize_rel(&root, input)?;
    ingest::update_config(home, |cfg| {
        let list = cfg["tracked_docs"].as_array().cloned().unwrap_or_default();
        let kept: Vec<Value> = list.iter().filter(|v| !same_entry(v, &root, &rel)).cloned().collect();
        anyhow::ensure!(kept.len() < list.len(), "未登记该文件：{rel}");
        cfg["tracked_docs"] = Value::Array(kept);
        Ok(())
    })?;
    Ok(json!({ "ok": true, "project": root, "path": rel }))
}

fn mtime_iso(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// 项目文档状态：每个候选文件的最后更新时间、备份修订数、写过它的对话、
/// 此后新开的对话数与过时提示。
pub fn status(conn: &Connection, home: &Path, project_id: i64) -> Result<Value> {
    let root = project_path(conn, project_id)?;
    let backups = db::list_memory_files(conn)?;
    let writes = project_writes(conn, project_id)?;
    let now = chrono::Utc::now();
    let mut docs = Vec::new();
    for (agent, abs, manual) in root_candidates(&tracked(home), &root) {
        let abs_str = abs.to_string_lossy().to_string();
        let key = db::path_key(&abs_str);
        let rel = abs.strip_prefix(&root).unwrap_or(&abs).to_string_lossy().replace('\\', "/");
        let backup = backups.iter()
            .filter(|f| f["path"].as_str().map(db::path_key).as_deref() == Some(key.as_str()))
            .max_by(|a, b| a["updated_at"].as_str().cmp(&b["updated_at"].as_str()));
        let exists = abs.is_file();
        let too_large = std::fs::metadata(&abs).map_or(false, |m| m.len() > MAX_DOC_BYTES);
        let last_updated = mtime_iso(&abs)
            .or_else(|| backup.and_then(|f| f["updated_at"].as_str().map(str::to_string)));

        // julianday 统一换算：各 agent 的时间戳可能带毫秒或时区偏移，字符串比较会错位
        let sessions_since: i64 = match &last_updated {
            Some(ts) => conn.query_row(
                "SELECT COUNT(*) FROM sessions s
                 WHERE s.project_id = ?1 AND s.deleted_at IS NULL AND s.message_count > 0
                   AND julianday(s.started_at) > julianday(?2)
                   AND NOT EXISTS (SELECT 1 FROM session_links l
                                   WHERE l.child_session_id = s.id AND l.link_type = 'subagent')",
                params![project_id, ts],
                |r| r.get(0),
            )?,
            None => 0,
        };
        let days_since = last_updated.as_deref()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|t| (now - t.with_timezone(&chrono::Utc)).num_days());
        let stale = exists
            && (sessions_since >= STALE_SESSIONS
                || (days_since.unwrap_or(0) >= STALE_DAYS && sessions_since >= 1));

        let edited: Vec<&Value> = writes.iter().filter(|(k, _)| *k == key).map(|(_, v)| v).collect();
        docs.push(json!({
            "name": rel,
            "path": abs_str,
            "agent": agent,
            "manual": manual,
            "exists": exists,
            "too_large": too_large,
            "file_id": backup.map(|f| f["id"].clone()),
            "revisions": backup.map(|f| f["revisions"].clone()).unwrap_or(json!(0)),
            "last_updated": last_updated,
            "days_since": days_since,
            "sessions_since": sessions_since,
            "stale": stale,
            "edited_by": edited.iter().take(5).collect::<Vec<_>>(),
            "edited_by_total": edited.len(),
        }));
    }
    Ok(json!({
        "docs": docs,
        "stale_sessions": STALE_SESSIONS,
        "stale_days": STALE_DAYS,
    }))
}

/// 附在上下文 / 卷宗里的版本：读取失败时只在这一节报错，不连带整份上下文失败。
pub fn attach(conn: &Connection, home: &Path, project_id: i64) -> Value {
    status(conn, home, project_id).unwrap_or_else(|e| json!({ "docs": [], "error": e.to_string() }))
}

/// 本项目对话写过的文件，按时间倒序：(路径键, {session_id, agent, at})。
/// artifact 路径可能是相对路径，按所属对话的 cwd 解析并按路径段规整后再取键。
/// 一次查询走 idx_artifacts_project，供所有候选文档共用。
fn project_writes(conn: &Connection, project_id: i64) -> Result<Vec<(String, Value)>> {
    let mut stmt = conn.prepare(
        "SELECT a.session_id, a.path, a.created_at, s.cwd, s.agent
         FROM session_artifacts a JOIN sessions s ON s.id = a.session_id
         WHERE a.project_id = ?1 AND s.deleted_at IS NULL
         ORDER BY julianday(a.created_at) DESC",
    )?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (sid, path, at, cwd, agent) = row?;
        let p = Path::new(&path);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            match &cwd {
                Some(c) => Path::new(c).join(p),
                None => continue,
            }
        };
        let key = db::path_key(&lexical_normalize(&joined).to_string_lossy());
        out.push((key, json!({ "session_id": sid, "agent": agent, "at": at })));
    }
    Ok(out)
}

/// 去掉 `.`、按 `..` 回退一级（纯路径运算，不访问磁盘）。
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalize_rel;

    #[test]
    fn normalize_rel_rejects_escape_and_accepts_inside_absolute() {
        assert!(normalize_rel("/p/x", "../secret.md").is_err());
        assert!(normalize_rel("/p/x", "").is_err());
        assert_eq!(normalize_rel("/p/x", "docs\\plan.md").unwrap(), "docs/plan.md");
        if cfg!(unix) {
            assert_eq!(normalize_rel("/p/x", "/p/x/docs/plan.md").unwrap(), "docs/plan.md");
            assert_eq!(normalize_rel("/p/x", "/p/x/plan.md/").unwrap(), "plan.md");
            assert!(normalize_rel("/p/x", "/p/xy/plan.md").is_err());
        } else {
            assert_eq!(normalize_rel(r"D:\p\x", r"d:\P\x\docs\plan.md").unwrap(), "docs/plan.md");
            assert_eq!(normalize_rel(r"D:\p\x", r"D:\p\x\plan.md\").unwrap(), "plan.md");
            assert!(normalize_rel(r"D:\p\x", r"D:\p\xy\plan.md").is_err());
        }
    }
}
