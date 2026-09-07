//! 备份根目录迁移：先完整迁移并校验，再切换 config.json。

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct CopyStats {
    files: u64,
    bytes: u64,
}

pub fn set(home: &Path, configured: &str) -> Result<Value> {
    let raw = configured.trim();
    let old = crate::backups_dir(home);
    let new = if raw.is_empty() {
        home.join("backups")
    } else {
        let path = PathBuf::from(crate::expand_home(raw));
        anyhow::ensure!(path.is_absolute(), "请填绝对路径：{raw}");
        path
    };

    if same_path(&old, &new) {
        fs::create_dir_all(&new).with_context(|| format!("目录不可用：{}", new.display()))?;
        save_config(home, raw)?;
        return Ok(json!({ "effective": new, "moved_files": 0, "moved_bytes": 0 }));
    }
    reject_nested(&old, &new)?;
    if old.exists() {
        anyhow::ensure!(old.is_dir(), "原备份位置不是文件夹：{}", old.display());
    }
    let destination_existed = new.exists();
    if destination_existed {
        anyhow::ensure!(new.is_dir(), "新备份位置不是文件夹：{}", new.display());
        anyhow::ensure!(
            fs::read_dir(&new)?.next().is_none(),
            "新备份位置不是空文件夹，请选择空目录：{}",
            new.display()
        );
    }
    let parent = new.parent().context("新备份位置没有父目录")?;
    fs::create_dir_all(parent).with_context(|| format!("无法创建父目录：{}", parent.display()))?;

    let stats = if old.exists() {
        dir_stats(&old)?
    } else {
        CopyStats::default()
    };
    if !old.exists() {
        fs::create_dir_all(&new)?;
        if let Err(e) = save_config(home, raw) {
            if !destination_existed {
                let _ = fs::remove_dir(&new);
            }
            return Err(e);
        }
        return Ok(json!({ "effective": new, "moved_files": 0, "moved_bytes": 0 }));
    }

    if destination_existed {
        fs::remove_dir(&new)?; // 已确认是空目录，便于整目录原子 rename
    }
    match fs::rename(&old, &new) {
        Ok(()) => {
            if let Err(e) = save_config(home, raw) {
                let _ = fs::rename(&new, &old);
                if destination_existed {
                    let _ = fs::create_dir_all(&new);
                }
                return Err(e);
            }
            Ok(
                json!({ "effective": new, "source": old, "moved_files": stats.files, "moved_bytes": stats.bytes }),
            )
        }
        Err(_) => copy_then_switch(home, raw, &old, &new, destination_existed),
    }
}

fn copy_then_switch(
    home: &Path,
    raw: &str,
    old: &Path,
    new: &Path,
    destination_existed: bool,
) -> Result<Value> {
    let name = new
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("backups");
    let stage = new.with_file_name(format!(".{name}.yourmem-migrate-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<CopyStats> {
        let stats = copy_verified(old, &stage)?;
        fs::rename(&stage, new)
            .with_context(|| format!("无法启用新备份位置：{}", new.display()))?;
        Ok(stats)
    })();
    let stats = match result {
        Ok(stats) => stats,
        Err(e) => {
            let _ = fs::remove_dir_all(&stage);
            if destination_existed {
                let _ = fs::create_dir_all(new);
            }
            return Err(e);
        }
    };
    if let Err(e) = save_config(home, raw) {
        let _ = fs::remove_dir_all(new);
        if destination_existed {
            let _ = fs::create_dir_all(new);
        }
        return Err(e);
    }
    let cleanup_warning = fs::remove_dir_all(old)
        .err()
        .map(|e| format!("新位置已生效，但原目录未能删除：{e}"));
    Ok(json!({
        "effective": new, "source": old, "moved_files": stats.files,
        "moved_bytes": stats.bytes, "cleanup_warning": cleanup_warning,
    }))
}

fn copy_verified(source: &Path, destination: &Path) -> Result<CopyStats> {
    fs::create_dir_all(destination)?;
    let mut stats = CopyStats::default();
    for entry in walkdir::WalkDir::new(source) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(source)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
            anyhow::ensure!(
                file_hash(entry.path())? == file_hash(&target)?,
                "迁移校验失败：{}",
                entry.path().display()
            );
            let len = entry.metadata()?.len();
            stats.files += 1;
            stats.bytes += len;
        } else {
            anyhow::bail!(
                "备份目录包含不支持的链接或特殊文件：{}",
                entry.path().display()
            );
        }
    }
    Ok(stats)
}

fn file_hash(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(digest.finalize().to_vec())
}

fn dir_stats(path: &Path) -> Result<CopyStats> {
    let mut stats = CopyStats::default();
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        if entry.file_type().is_file() {
            stats.files += 1;
            stats.bytes += entry.metadata()?.len();
        }
    }
    Ok(stats)
}

fn save_config(home: &Path, raw: &str) -> Result<()> {
    let mut cfg = crate::ingest::read_config(home);
    if raw.is_empty() {
        if let Some(obj) = cfg.as_object_mut() {
            obj.remove("backup_dir");
        }
    } else {
        cfg["backup_dir"] = json!(raw);
    }
    crate::ingest::write_config(home, &cfg)
}

fn normalized(path: &Path) -> String {
    let value = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = value
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalized(a) == normalized(b)
}

fn reject_nested(old: &Path, new: &Path) -> Result<()> {
    let old = normalized(old);
    let new = normalized(new);
    anyhow::ensure!(
        !new.starts_with(&(old.clone() + "/")) && !old.starts_with(&(new.clone() + "/")),
        "新旧备份位置不能互相包含"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_existing_backups_before_switching_config() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let target = root.path().join("moved");
        fs::create_dir_all(home.join("backups/db")).unwrap();
        fs::write(home.join("backups/db/snapshot.sqlite"), b"backup").unwrap();
        let result = set(&home, target.to_str().unwrap()).unwrap();
        assert_eq!(result["moved_files"], 1);
        assert_eq!(
            fs::read(target.join("db/snapshot.sqlite")).unwrap(),
            b"backup"
        );
        assert!(!home.join("backups").exists());
        assert_eq!(crate::backups_dir(&home), target);
    }

    #[test]
    fn nonempty_destination_is_rejected_without_changing_source_or_config() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let target = root.path().join("occupied");
        fs::create_dir_all(home.join("backups")).unwrap();
        fs::write(home.join("backups/keep.tar.gz"), b"keep").unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("other.txt"), b"other").unwrap();
        assert!(set(&home, target.to_str().unwrap()).is_err());
        assert!(home.join("backups/keep.tar.gz").exists());
        assert_eq!(crate::backups_dir(&home), home.join("backups"));
    }

    #[test]
    fn verified_copy_preserves_nested_files_and_counts() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(source.join("snapshots-v1/objects")).unwrap();
        fs::write(source.join("one.tar.gz"), b"one").unwrap();
        fs::write(source.join("snapshots-v1/objects/two"), b"two-two").unwrap();
        let stats = copy_verified(&source, &destination).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.bytes, 10);
        assert_eq!(fs::read(destination.join("one.tar.gz")).unwrap(), b"one");
        assert_eq!(
            fs::read(destination.join("snapshots-v1/objects/two")).unwrap(),
            b"two-two"
        );
    }
}
