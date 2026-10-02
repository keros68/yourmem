//! Explicit local cleanup used before uninstall. Agent-owned source histories
//! are never in scope; only yourmem's selected data, backups and location
//! pointer are reported and removed.

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn safe_directory(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute() && path.parent().is_some(),
        "拒绝清理磁盘根目录"
    );
    let resolved = if path.exists() { path.canonicalize()? } else { path.to_path_buf() };
    let user_home = crate::home_dir().canonicalize().unwrap_or_else(|_| crate::home_dir());
    ensure!(!user_home.starts_with(&resolved), "拒绝清理用户主目录或其上级目录");
    if let Ok(cwd) = std::env::current_dir().and_then(|p| p.canonicalize()) {
        ensure!(!cwd.starts_with(&resolved), "拒绝清理当前工作区或其上级目录");
    }
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        ensure!(!exe.starts_with(&resolved), "拒绝清理程序所在目录或其上级目录");
    }
    Ok(resolved)
}

pub fn plan(home: &Path, include_backups: bool) -> Result<Value> {
    let data = safe_directory(home)?;
    let backup = safe_directory(&crate::backups_dir(home))?;
    ensure!(!data.starts_with(&backup), "备份目录不能包含核心数据目录");
    let backup_separate = !backup.starts_with(&data);
    // 备份在数据目录内且选择保留：删除数据目录时绕开备份子树
    let backups_kept_inside = !include_backups && !backup_separate && backup.exists();
    let data_bytes = if backups_kept_inside { crate::dir_bytes(home).saturating_sub(crate::dir_bytes(&backup)) } else { crate::dir_bytes(home) };
    let payload = json!({
        "data_dir": data,
        "data_bytes": data_bytes,
        "backup_dir": backup,
        "backup_bytes": if include_backups && backup_separate { crate::dir_bytes(&backup) } else { 0 },
        "include_backups": include_backups,
        "backup_separate": backup_separate,
        "backups_kept_inside": backups_kept_inside,
        "location_pointer": crate::data_home_pointer(),
        "preserved": [crate::default_claude_root(), crate::default_codex_root(), crate::default_zcode_root(), crate::default_kimi_root(), crate::default_pi_root(), crate::adapters::opencode::default_db_path(), crate::adapters::hermes::default_db_path()],
        "preserved_scope": "所有 agent 自有会话与用户配置中除 yourmem 条目外的内容",
    });
    let token = format!("{:x}", Sha256::digest(serde_json::to_vec(&payload)?));
    Ok(json!({"token":token,"cleanup":payload}))
}

/// 先把整个数据目录改名移开，再删除。Windows 上目录里有文件被打开时改名会
/// 直接失败，这样要么一个文件都不删，要么删掉的是已无人使用的完整目录，
/// 不会留下"有索引、无原件"的残缺资料库。备份位于数据目录内且要保留时，
/// 改名后先把备份移回原位置。
fn remove_data_dir(data: &Path, keep_backup: Option<&Path>) -> Result<()> {
    if !data.exists() {
        return Ok(());
    }
    let name = data.file_name().context("数据目录没有名称")?.to_string_lossy().to_string();
    let moved = data.with_file_name(format!("{name}.removing-{}", std::process::id()));
    std::fs::rename(data, &moved).with_context(|| {
        format!("资料库正在被使用（采集或其他操作），请稍后重试；未删除任何文件：{}", data.display())
    })?;
    if let Some(backup) = keep_backup {
        let rel = backup.strip_prefix(data).context("备份目录不在数据目录内")?;
        let inside = moved.join(rel);
        if inside.exists() {
            if let Some(parent) = backup.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&inside, backup)
                .with_context(|| format!("移回备份目录 {}", backup.display()))?;
        }
    }
    std::fs::remove_dir_all(&moved).with_context(|| format!("删除核心数据目录 {}", moved.display()))
}

pub fn execute(home: &Path, include_backups: bool, token: &str) -> Result<Value> {
    let current = plan(home, include_backups)?;
    ensure!(
        current["token"].as_str() == Some(token),
        "清理目标已变化，请重新预览"
    );
    let data = PathBuf::from(current["cleanup"]["data_dir"].as_str().context("清理计划缺少数据目录")?);
    let backup = PathBuf::from(current["cleanup"]["backup_dir"].as_str().context("清理计划缺少备份目录")?);
    let backup_separate = current["cleanup"]["backup_separate"].as_bool().unwrap_or(false);
    let backups_kept_inside = current["cleanup"]["backups_kept_inside"].as_bool().unwrap_or(false);
    let active_home = safe_directory(&crate::data_home()).ok();
    remove_data_dir(&data, backups_kept_inside.then_some(backup.as_path()))?;
    if active_home.as_ref() == Some(&data) {
        crate::clear_data_home_pointer()?;
    }
    if include_backups && backup_separate && backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("删除备份目录 {}", backup.display()))?;
    }
    Ok(
        json!({"removed_data":data,"removed_backups":include_backups && backup_separate,"backups_kept_inside":backups_kept_inside,"backup_dir":backup}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_is_token_gated_and_keeps_agent_sources() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("yourmem-data");
        let backup = dir.path().join("yourmem-backup");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(home.join("yourmem.db"), b"db").unwrap();
        std::fs::write(backup.join("one.tar.gz"), b"backup").unwrap();
        crate::ingest::write_config(&home, &json!({"backup_dir":backup})).unwrap();
        let p = plan(&home, true).unwrap();
        assert!(execute(&home, true, "wrong").is_err());
        execute(&home, true, p["token"].as_str().unwrap()).unwrap();
        assert!(!home.exists() && !backup.exists());
    }

    #[test]
    fn cleanup_keeps_backups_inside_data_dir_when_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("yourmem-data");
        let backup = home.join("backups");
        std::fs::create_dir_all(backup.join("db")).unwrap();
        std::fs::create_dir_all(home.join("objects").join("ab")).unwrap();
        std::fs::write(home.join("yourmem.db"), b"db").unwrap();
        std::fs::write(home.join("objects").join("ab").join("x"), b"obj").unwrap();
        std::fs::write(backup.join("db").join("snap.db"), b"snapshot").unwrap();
        let p = plan(&home, false).unwrap();
        assert_eq!(p["cleanup"]["backups_kept_inside"], true);
        execute(&home, false, p["token"].as_str().unwrap()).unwrap();
        assert!(backup.join("db").join("snap.db").exists());
        assert!(!home.join("yourmem.db").exists() && !home.join("objects").exists());

        let p = plan(&home, true).unwrap();
        execute(&home, true, p["token"].as_str().unwrap()).unwrap();
        assert!(!home.exists());
    }

    #[cfg(windows)]
    #[test]
    fn an_open_database_stops_cleanup_before_anything_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("yourmem-data");
        let backup = dir.path().join("yourmem-backup");
        std::fs::create_dir_all(&backup).unwrap();
        let conn = crate::db::open(&home).unwrap();
        crate::vault::store_line(&home, b"line").unwrap();
        crate::ingest::write_config(&home, &json!({"backup_dir": backup})).unwrap();
        let p = plan(&home, false).unwrap();
        assert!(execute(&home, false, p["token"].as_str().unwrap()).is_err());
        assert!(home.join("yourmem.db").exists() && home.join("objects.db").exists() && home.join("config.json").exists(),
            "被占用时一个文件都不删");
        drop(conn);
        let p = plan(&home, false).unwrap();
        execute(&home, false, p["token"].as_str().unwrap()).unwrap();
        assert!(!home.exists());
    }
}
