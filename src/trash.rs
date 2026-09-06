//! 清除用例。确认回调在导入锁内运行，预览与执行使用同一集合。
//! CLI 保留前置 sweep；桌面单条/选中批量保留后置 sweep 与档案移除策略。
use std::{path::Path, time::Duration};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::{db, ingest::ImportLockTx};

pub fn purge_cli(
    conn: &mut Connection, home: &Path, id: &str, force: bool, keep_archive: bool,
    confirm: impl FnOnce(&Value) -> Result<()>,
) -> Result<Value> {
    let _lock = ImportLockTx::acquire(home, Duration::from_secs(30))?;
    db::gc_sweep(conn, home)?;
    let plan = db::purge_plan(conn, home, id)?;
    confirm(&plan)?;
    db::purge_session(conn, home, id, force, !keep_archive)
}

pub fn purge_desktop(conn: &mut Connection, home: &Path, id: &str, force: bool) -> Result<Value> {
    let _lock = ImportLockTx::acquire(home, Duration::from_secs(30))?;
    let _ = db::log_usage(conn, "app", "session purge");
    let result = db::purge_session(conn, home, id, force, true)?;
    sweep_and_drop(conn, home)?;
    Ok(result)
}

pub fn purge_selected(conn: &mut Connection, home: &Path, ids: &[String], force: bool) -> Result<Value> {
    let _lock = ImportLockTx::acquire(home, Duration::from_secs(120))?;
    let _ = db::log_usage(conn, "app", "trash purge selected");
    let mut purged = 0usize;
    let mut failed = Vec::new();
    for id in ids {
        match db::purge_session(conn, home, id, force, true) {
            Ok(_) => purged += 1,
            Err(e) => failed.push(json!({ "id": id, "error": e.to_string() })),
        }
    }
    sweep_and_drop(conn, home)?;
    Ok(json!({ "purged": purged, "failed": failed }))
}

fn sweep_and_drop(conn: &Connection, home: &Path) -> Result<()> {
    let sweep = db::gc_sweep(conn, home)?;
    if let Some(dir) = sweep["backup_dir"].as_str() {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            eprintln!("yourmem purge: 共享对象档案移除失败（档案保留，可在设置页清理）: {e}");
        }
    }
    Ok(())
}

pub fn empty_cli(
    conn: &mut Connection, home: &Path, confirm: impl FnOnce(&Value) -> Result<()>,
) -> Result<Value> {
    empty_overdue(conn, home, Duration::from_secs(30), false, false, confirm)
}

pub fn empty_desktop(conn: &mut Connection, home: &Path) -> Result<Value> {
    empty_overdue(conn, home, Duration::from_secs(30), true, true, |_| Ok(()))
}

pub fn empty_on_startup(conn: &mut Connection, home: &Path) -> Result<Value> {
    empty_overdue(conn, home, Duration::from_secs(5), true, false, |_| Ok(()))
}

fn empty_overdue(
    conn: &mut Connection, home: &Path, timeout: Duration, sweep_after: bool, log: bool,
    confirm: impl FnOnce(&Value) -> Result<()>,
) -> Result<Value> {
    let _lock = ImportLockTx::acquire(home, timeout)?;
    db::gc_sweep(conn, home)?;
    if log { let _ = db::log_usage(conn, "app", "trash empty"); }
    let plan = db::trash_overdue_plan(conn, home)?;
    let ids: Vec<String> = plan["overdue_ids"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Ok(json!({ "purged_sessions": 0, "note": "回收站没有超期会话" }));
    }
    confirm(&plan)?;
    let result = db::purge_trash(conn, home, &ids)?;
    if sweep_after { db::gc_sweep(conn, home)?; }
    Ok(result)
}
