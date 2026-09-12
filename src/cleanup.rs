//! Explicit local cleanup used before uninstall. Agent-owned source histories
//! are never in scope; only yourmem's selected data, backups and location
//! pointer are reported and removed.

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn bytes(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

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
    let payload = json!({
        "data_dir": data,
        "data_bytes": bytes(home),
        "backup_dir": backup,
        "backup_bytes": if include_backups && backup_separate { bytes(&backup) } else { 0 },
        "include_backups": include_backups,
        "backup_separate": backup_separate,
        "location_pointer": crate::data_home_pointer(),
        "preserved": [crate::default_claude_root(), crate::default_codex_root(), crate::default_zcode_root(), crate::default_kimi_root(), crate::default_pi_root(), crate::adapters::opencode::default_db_path(), crate::adapters::hermes::default_db_path()],
        "preserved_scope": "所有 agent 自有会话与用户配置中除 yourmem 条目外的内容",
    });
    let token = format!("{:x}", Sha256::digest(serde_json::to_vec(&payload)?));
    Ok(json!({"token":token,"cleanup":payload}))
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
    let active_home = safe_directory(&crate::data_home()).ok();
    if data.exists() {
        std::fs::remove_dir_all(&data)
            .with_context(|| format!("删除核心数据目录 {}", data.display()))?;
    }
    if active_home.as_ref() == Some(&data) {
        crate::clear_data_home_pointer()?;
    }
    if include_backups && backup_separate && backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("删除备份目录 {}", backup.display()))?;
    }
    Ok(
        json!({"removed_data":data,"removed_backups":include_backups && backup_separate,"backup_dir":backup}),
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
}
