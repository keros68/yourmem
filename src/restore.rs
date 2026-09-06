//! restore-agents: write a session's raw file back into the agent's data dir
//! (DESIGN-0.3 §5.2). 设计哲学是知情门控而非禁止：
//!   1. 写前打印完整路径清单与后果（`plan`）；
//!   2. 目标已存在默认拒绝；`--force` 时先写 `.bak-YYYYMMDD-HHMMSS` 副本；
//!   3. 字节级重建（vault export 逐对象哈希校验），不做任何格式转换；
//!   4. 一次性迁移操作，不是同步。
//! 仅限文件型 agent（claude/codex）：SQLite 型（opencode 等）不向别人的库里
//! 插行，那是真的不可控，不是原则洁癖。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::{adapters, vault};

/// Agents whose sessions are plain JSONL files (byte-level rebuild is safe).
pub const FILE_BASED_AGENTS: [&str; 2] = [adapters::AGENT_CLAUDE, adapters::AGENT_CODEX];

struct SessionRow {
    id: String,
    agent: String,
    native_id: String,
    file_path: String,
}

fn lookup_session(conn: &Connection, agent: &str, session: &str) -> Result<SessionRow> {
    // Accept both the full key ("claude:<uuid>") and the bare native id.
    let key = if session.contains(':') { session.to_string() } else { format!("{agent}:{session}") };
    let row = conn
        .query_row(
            "SELECT id, agent, native_id, file_path FROM sessions WHERE id = ?1",
            params![key],
            |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    agent: r.get(1)?,
                    native_id: r.get(2)?,
                    file_path: r.get(3)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("session not found: {key}"))?;
    anyhow::ensure!(
        FILE_BASED_AGENTS.contains(&row.agent.as_str()),
        "agent {} 的会话存在 SQLite 里，不支持写回（只读原则：不向别人的库插行）；可恢复进 yourmem（bundle restore）",
        row.agent
    );
    Ok(row)
}

/// 门控要素 1：预览——将写入的完整路径与后果。
pub fn plan(conn: &Connection, agent: &str, session: &str) -> Result<Value> {
    let s = lookup_session(conn, agent, session)?;
    let target = PathBuf::from(&s.file_path);
    let lines: i64 = conn.query_row(
        "SELECT COUNT(*) FROM vault_lines WHERE session_id = ?1",
        params![s.id],
        |r| r.get(0),
    )?;
    anyhow::ensure!(lines > 0, "会话 {} 没有 vault 归档，无法重建", s.id);
    Ok(json!({
        "session_id": s.id,
        "agent": s.agent,
        "will_write": [s.file_path],
        "target_exists": target.is_file(),
        "vault_lines": lines,
        "resume_command": adapters::resume_command(&s.agent, &s.native_id),
        "consequences": "将按 vault 归档逐字节重建原始会话文件，写入上述 agent 数据目录；目标已存在时需 --force，覆盖前自动做 .bak 时间戳备份。这是一次性迁移，不是同步。",
    }))
}

/// 追加式后缀，完整保留原文件名（`a.jsonl` → `a.jsonl.bak-…`）。
/// 不能用 `with_extension`：它是替换最后一个扩展名，会把 `.jsonl` 吃掉。
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// 门控要素 2/3：确认后的执行。`confirmed` 由调用方（CLI 交互或 UI 弹窗）保证。
pub fn execute(conn: &Connection, home: &Path, agent: &str, session: &str, force: bool) -> Result<Value> {
    let p = plan(conn, agent, session)?;
    let sid = p["session_id"].as_str().unwrap().to_string();
    let target = PathBuf::from(p["will_write"][0].as_str().unwrap());

    let mut backup = Value::Null;
    if target.is_file() {
        anyhow::ensure!(
            force,
            "目标已存在：{}。默认拒绝覆盖；确认要覆盖请加 --force（会先自动备份 .bak）",
            target.display()
        );
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let bak = with_suffix(&target, &format!("bak-{ts}"));
        std::fs::copy(&target, &bak)
            .with_context(|| format!("备份目标文件到 {}", bak.display()))?;
        backup = json!(bak);
    } else {
        // 换机恢复场景：目标目录可能不存在
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // 写 tmp 再 rename：写回也不留半截文件（同 vault 写盘纪律）；
    // 失败时清理 tmp——半截文件躺在 agent 数据目录里会吓到人（自检 B5）
    let tmp = with_suffix(&target, &format!("yourmem-tmp.{}", std::process::id()));
    let lines = match vault::export_session(conn, home, &sid, &tmp) {
        Ok(l) => l,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, &target)?;

    Ok(json!({
        "restored": target,
        "backup": backup,
        "lines_written": lines,
        "resume_command": p["resume_command"],
        "note": "一次性迁移完成；这不是同步。",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bak_and_tmp_names_keep_original_extension() {
        let p = Path::new("/tmp/x/session-abc.jsonl");
        assert_eq!(
            with_suffix(p, "bak-20260823-235959"),
            PathBuf::from("/tmp/x/session-abc.jsonl.bak-20260823-235959")
        );
        assert_eq!(
            with_suffix(p, "yourmem-tmp.123"),
            PathBuf::from("/tmp/x/session-abc.jsonl.yourmem-tmp.123")
        );
    }
}
