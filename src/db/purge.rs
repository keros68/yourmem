//! 回收站清除的数据库事务与对象归档。生产入口由 crate::trash 持锁编排。
use std::path::Path;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use crate::now_iso;

/// 回收站默认保留期（§6）：软删后 30 天内拒绝物理清除。
pub const TRASH_RETENTION_DAYS: i64 = 30;

/// 物理清除预览（§6 双重确认的第一道：打印将删除的对象数与字节数）。
pub fn purge_plan(conn: &Connection, home: &Path, session_id: &str) -> Result<Value> {
    let row: Option<(Option<String>, i64)> = conn
        .query_row(
            "SELECT deleted_at, message_count FROM sessions WHERE id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((deleted_at, messages)) = row else {
        anyhow::bail!("session not found: {session_id}");
    };
    let Some(deleted_at) = deleted_at else {
        return Ok(json!({
            "session_id": session_id, "in_trash": false,
            "note": "会话不在回收站（先 session delete），物理清除只对回收站内容开放",
        }));
    };
    let deleted_ts = chrono::DateTime::parse_from_rfc3339(&deleted_at)
        .context("invalid deleted_at")?
        .with_timezone(&chrono::Utc);
    let age_days = (chrono::Utc::now() - deleted_ts).num_days();
    let overdue = age_days >= TRASH_RETENTION_DAYS;
    let (vault_lines, artifacts): (i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM vault_lines WHERE session_id = ?1),
                (SELECT COUNT(*) FROM session_artifacts WHERE session_id = ?1)",
        params![session_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // 独占对象（将被归档的）：数量 + 实测字节数（§6"打印对象数与字节数"）
    let exclusive: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT hash FROM (
                SELECT hash FROM vault_lines WHERE session_id = ?1
                EXCEPT SELECT hash FROM vault_lines WHERE session_id != ?1
                EXCEPT SELECT hash FROM memory_revisions)",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let exclusive_objects = exclusive.len() as i64;
    let exclusive_bytes: u64 = exclusive
        .iter()
        .map(|h| std::fs::metadata(crate::vault::object_path(home, h)).map(|m| m.len()).unwrap_or(0))
        .sum();
    Ok(json!({
        "session_id": session_id, "in_trash": true,
        "deleted_at": deleted_at, "age_days": age_days,
        "can_purge": overdue,
        "remaining_days": if overdue { 0 } else { TRASH_RETENTION_DAYS - age_days },
        "messages": messages, "vault_lines": vault_lines, "artifacts": artifacts,
        "exclusive_objects": exclusive_objects, "exclusive_bytes": exclusive_bytes,
        "backup": "执行时先归档：会话行导出 JSON、独占对象移入 backups/purge/（预览→确认→备份→执行）",
        "consequences": "物理删除：messages/FTS/vault 清单/会话行立即消失，独占的 CAS 对象按引用计数回收（跨会话共享对象保留）。不可恢复。",
    }))
}

/// 物理清除一个会话（已通过 plan 门控与保留期检查后调用）。
/// 四要素的"备份→执行"：会话行导出 JSON、独占对象 rename 进
/// backups/purge/<session>-<ts>/；候选先入 gc_pending 队列再逐个归档，
/// 中途失败由 gc_sweep 收尾（幂等可重入）；purged_sources 墓碑阻止
/// 后续 import 把被清文件重新导回来（游标已删，会完整重导——必须挡）。
/// session_id（`agent:native_id`）拼进备份目录名前的净化：Windows 文件名
/// 禁 `:*?"<>|/\`（冒号必现于 `agent:id`）。三端同处理保持一致（先例：桌面
/// session_export 的 `:` → `_`）；目录只是归档容器，无按名反查逻辑，且带毫秒
/// 时间戳，净化成 `_` 的理论碰撞无实际影响。
fn slug_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|' | '/' | '\\') { '_' } else { c })
        .collect()
}

/// force=true 跳过保留期门控（用户裁定 2026-09-04「把权力交给用户」：保留期内
/// 也允许知情强制清除，UI/CLI 二次确认承担知情）；备份归档语义不变——仍先
/// 导出 rows.json + 对象归档，再从主库移除。
/// drop_archive=true 在清除完成后把本次归档目录一并移除（1.0.0，「真删」：
/// 主库与磁盘字节都不再保留；过程仍是先归档再删——中途崩溃只会留下本应被
/// 删的档案，不会丢数据，残留目录可在设置页档案管理清理）。
pub fn purge_session(conn: &mut Connection, home: &Path, session_id: &str, force: bool, drop_archive: bool) -> Result<Value> {
    let plan = purge_plan(conn, home, session_id)?;
    anyhow::ensure!(plan["in_trash"] == true, "会话不在回收站：{session_id}");
    if !force {
        anyhow::ensure!(plan["can_purge"] == true,
            "保留期内：还剩 {} 天（回收站默认保留 {} 天）", plan["remaining_days"], TRASH_RETENTION_DAYS);
    }

    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f");
    let backup_dir = crate::backups_dir(home).join("purge").join(format!("{}-{ts}", slug_session_id(session_id)));
    std::fs::create_dir_all(&backup_dir)?;

    let (agent, file_path): (String, String) = conn.query_row(
        "SELECT agent, file_path FROM sessions WHERE id = ?1",
        params![session_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let tx = conn.transaction()?;
    // (a) 备份：被删的库行导出 JSON（恢复的原料；CAS 对象随后 rename 进同目录）
    let dump = dump_session_rows(&tx, session_id)?;
    std::fs::write(backup_dir.join("rows.json"), serde_json::to_string_pretty(&dump)?)?;
    // (b) 候选对象入 GC 队列（删行后清单就没了）
    let candidates: Vec<String> = {
        let mut stmt = tx.prepare("SELECT DISTINCT hash FROM vault_lines WHERE session_id = ?1")?;
        let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let now = now_iso();
    for h in &candidates {
        tx.execute("INSERT OR IGNORE INTO gc_pending(hash, added_at) VALUES (?1, ?2)", params![h, now])?;
    }
    let removed_messages: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id = ?1", params![session_id], |r| r.get(0))?;
    // (c) 删行（FTS 触发器联动；session_links 两端清）
    tx.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id])?;
    tx.execute("DELETE FROM vault_lines WHERE session_id = ?1", params![session_id])?;
    tx.execute("DELETE FROM session_uuids WHERE session_id = ?1", params![session_id])?;
    tx.execute("DELETE FROM session_artifacts WHERE session_id = ?1", params![session_id])?;
    tx.execute(
        "DELETE FROM session_links WHERE child_session_id = ?1 OR parent_session_id = ?1",
        params![session_id],
    )?;
    tx.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
    // (d) 游标清 + 墓碑立（阻止 import 完整重导复活）。游标按 (agent, path)：
    // 同一路径可能还有别的 agent 在采，只清本会话所属 agent 的
    tx.execute(
        "DELETE FROM source_files WHERE agent = ?1 AND path = ?2",
        params![agent, file_path],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO purged_sources(path, session_id, purged_at) VALUES (?1, ?2, ?3)",
        params![file_path, session_id, now],
    )?;
    tx.commit()?;

    // (e) 归档式 GC：rename 进备份目录（不是 unlink——§5.2 备份→执行语义）
    let (removed, kept) = archive_objects(conn, home, &backup_dir.join("objects"))?;
    // (f) 真删通道：本次归档目录随清除一并移除（尽力而为；失败仅少删一个
    // 目录，档案仍在设置页可见可清，不回滚主库删除）
    if drop_archive {
        if let Err(e) = std::fs::remove_dir_all(&backup_dir) {
            eprintln!("yourmem purge: 归档目录移除失败（档案保留）: {e}");
        }
    }
    Ok(json!({
        "purged": session_id,
        "removed_messages": removed_messages,
        "archived_objects": removed,
        "kept_shared_objects": kept,
        "backup_dir": backup_dir,
    }))
}

/// 被清会话的库行导出（恢复原料）。
fn dump_session_rows(conn: &Connection, session_id: &str) -> Result<Value> {
    let grab = |sql: &str| -> Result<Value> {
        let stmt = conn.prepare(sql)?;
        let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<Value> = stmt
            .query_map(params![session_id], |r| {
                let mut obj = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    let v: rusqlite::types::Value = r.get(i)?;
                    obj.insert(c.clone(), rusqlite_value_to_json(v));
                }
                Ok(Value::Object(obj))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Value::Array(rows))
    };
    Ok(json!({
        "session": grab("SELECT * FROM sessions WHERE id = ?1")?,
        "messages": grab("SELECT * FROM messages WHERE session_id = ?1")?,
        "vault_lines": grab("SELECT * FROM vault_lines WHERE session_id = ?1")?,
        "session_uuids": grab("SELECT * FROM session_uuids WHERE session_id = ?1")?,
        "session_artifacts": grab("SELECT * FROM session_artifacts WHERE session_id = ?1")?,
        // 谱系边（两端都可能指向本会话）与导入游标——终审 blocker：备份号称
        // 恢复原料却漏了这两块，purge 会永久丢 t6 谱系数据
        "session_links": grab("SELECT * FROM session_links WHERE child_session_id = ?1 OR parent_session_id = ?1")?,
        "source_file": grab("SELECT * FROM source_files WHERE path = (SELECT file_path FROM sessions WHERE id = ?1)")?,
    }))
}

fn rusqlite_value_to_json(v: rusqlite::types::Value) -> Value {
    use rusqlite::types::Value as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => json!(i),
        V::Real(f) => json!(f),
        V::Text(t) => json!(t),
        V::Blob(b) => json!(b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
    }
}

/// 对象移入归档目录：优先 rename（同盘瞬时）；失败（备份位置与数据目录
/// 跨盘，Windows 报 os error 17）退化为 copy_fallback。
fn move_object(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_fallback(src, dst)
}

/// 跨盘退路：先 copy 进同目录 tmp 再落位——目标只以完整形态出现，中途崩溃
/// 源未动、gc_pending 仍在，下轮 sweep 重试，不产生半截归档对象
/// （「先归档再删」语义在跨盘下不变）。
fn copy_fallback(src: &Path, dst: &Path) -> Result<()> {
    let tmp = dst.with_extension(format!("tmp.{}", std::process::id()));
    if let Err(e) = std::fs::copy(src, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    std::fs::rename(&tmp, dst)?;
    std::fs::remove_file(src)?;
    Ok(())
}

/// 归档式 GC：处理整个 gc_pending 队列（本会话的候选已入队）——对每个
/// hash 双重引用复查（vault_lines ∪ memory_revisions），无引用则移入
/// 备份目录（move_object：同盘 rename / 跨盘 copy）并出队；有引用则保留
/// 出队；对象已不在则直接出队（幂等）。
fn archive_objects(conn: &Connection, home: &Path, backup_dir: &Path) -> Result<(u64, u64)> {
    let pending: Vec<String> = {
        let mut stmt = conn.prepare("SELECT hash FROM gc_pending")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut removed = 0u64;
    let mut kept = 0u64;
    for hash in pending {
        let referenced: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM vault_lines WHERE hash = ?1)
                + EXISTS(SELECT 1 FROM memory_revisions WHERE hash = ?1)",
            params![hash],
            |r| r.get(0),
        )?;
        let src = crate::vault::object_path(home, &hash);
        if referenced > 0 || !src.is_file() {
            if referenced > 0 { kept += 1; }
            conn.execute("DELETE FROM gc_pending WHERE hash = ?1", params![hash])?;
            continue;
        }
        // unlink 前最后一刻再查（archive_objects 无锁窗口的兜底；公共互斥由
        // 调用方 ImportLockTx 提供，bundle merge 侧同样持锁——两侧都盖住）
        let referenced_now: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM vault_lines WHERE hash = ?1)
                + EXISTS(SELECT 1 FROM memory_revisions WHERE hash = ?1)",
            params![hash],
            |r| r.get(0),
        )?;
        if referenced_now > 0 {
            kept += 1;
            conn.execute("DELETE FROM gc_pending WHERE hash = ?1", params![hash])?;
            continue;
        }
        let shard = hash.get(..2).unwrap_or(&hash);
        std::fs::create_dir_all(backup_dir.join(shard))?;
        let dst = backup_dir.join(shard).join(&hash);
        if dst.exists() {
            std::fs::remove_file(&src)?;
        } else {
            move_object(&src, &dst)?;
        }
        conn.execute("DELETE FROM gc_pending WHERE hash = ?1", params![hash])?;
        removed += 1;
    }
    Ok((removed, kept))
}

/// 清 GC 队列残留（上次 purge 中途崩溃的尾巴）：任何 purge/trash empty 前
/// 自动跑一遍。对象归档到 backups/purge/sweep-<ts>/。
pub fn gc_sweep(conn: &Connection, home: &Path) -> Result<Value> {
    let pending: i64 = conn.query_row("SELECT COUNT(*) FROM gc_pending", [], |r| r.get(0))?;
    if pending == 0 {
        return Ok(json!({ "swept": 0 }));
    }
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f");
    let dir = crate::backups_dir(home).join("purge").join(format!("sweep-{ts}"));
    std::fs::create_dir_all(&dir)?;
    let (removed, kept) = archive_objects(conn, home, &dir.join("objects"))?;
    Ok(json!({ "swept": removed, "kept_shared": kept, "backup_dir": dir }))
}

/// 清空回收站里所有超期会话（同样走保留期门控）。
pub fn trash_overdue_plan(conn: &Connection, home: &Path) -> Result<Value> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(TRASH_RETENTION_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let overdue: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM sessions WHERE deleted_at IS NOT NULL AND deleted_at <= ?1",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    // 集合语义的独占对象（codex 八审）：逐会话求和会把"待清会话互相引用"
    // 的共享对象算成 0——按超期集合整体算：集合 UNION 的对象里，除开
    // 非集合会话与 memory_revisions 仍引用的，就是联合独占。
    let joint_exclusive: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT hash FROM (
                SELECT v.hash FROM vault_lines v JOIN sessions s ON s.id = v.session_id
                    WHERE s.deleted_at IS NOT NULL AND s.deleted_at <= ?1
                EXCEPT SELECT v.hash FROM vault_lines v JOIN sessions s ON s.id = v.session_id
                    WHERE NOT (s.deleted_at IS NOT NULL AND s.deleted_at <= ?1)
                EXCEPT SELECT hash FROM memory_revisions)",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let joint_bytes: u64 = joint_exclusive
        .iter()
        .map(|h| std::fs::metadata(crate::vault::object_path(home, h)).map(|m| m.len()).unwrap_or(0))
        .sum();
    let mut plans = Vec::new();
    for sid in &overdue {
        let p = purge_plan(conn, home, sid)?;
        plans.push(json!({ "session_id": sid, "messages": p["messages"], "vault_lines": p["vault_lines"] }));
    }
    Ok(json!({
        "overdue_sessions": plans,
        "overdue_ids": overdue,
        "exclusive_objects": joint_exclusive.len(),
        "exclusive_bytes": joint_bytes,
        "note": "对象数为超期集合的联合独占（互相共享的对象在最后一个会话清除时归档）",
    }))
}

/// 清空超期项。会话列表由调用方传入（trash_overdue_plan 的产物）——执行
/// 集合 = 确认过的集合，不再重算（codex 九审：确认期间刚跨过阈值的会话
/// 若被重算纳入，就绕过了预览→确认门控）。purge_session 内部仍做保留期
/// 校验（集合内会话当时已超期，时间只会更超）。
pub fn purge_trash(conn: &mut Connection, home: &Path, ids: &[String]) -> Result<Value> {
    let mut results = Vec::new();
    for sid in ids {
        results.push(purge_session(conn, home, sid, false, false)?);
    }
    Ok(json!({ "purged_sessions": results.len(), "results": results }))
}



#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn move_object_renames_in_place_and_copy_fallback_completes() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let p = dir.path().join(name).join("obj");
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            p
        };
        // rename 主路径（同目录树）：内容到达、源移除
        let src = mk("a");
        std::fs::write(&src, b"payload").unwrap();
        let dst = mk("b");
        move_object(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
        // copy 退路（跨盘 rename 失败时走的分支）：内容到达、源移除、无 tmp 残留
        let src2 = mk("c");
        std::fs::write(&src2, b"payload2").unwrap();
        let dst2 = mk("d");
        copy_fallback(&src2, &dst2).unwrap();
        assert!(!src2.exists());
        assert_eq!(std::fs::read(&dst2).unwrap(), b"payload2");
        assert_eq!(
            std::fs::read_dir(dst2.parent().unwrap()).unwrap().count(),
            1,
            "归档目录只留对象本身，tmp 已清理"
        );
    }

}
