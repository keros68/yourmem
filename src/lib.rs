//! yourmem — local-first session vault, memory and recall for AI coding agents.
//!
//! Layers (see docs/DESIGN.md):
//!   Raw session (read-only agent files)
//!     -> Normalized session (SQLite, unified schema)
//!     -> Memory / Handoff (curated, provenance-tracked)
//!   + Vault (line-level content-addressed immutable backup)

pub mod adapters;
pub mod backup_location;
pub mod bundle;
pub mod db;
pub mod doctor;
pub mod dossier;
pub mod ingest;
pub mod mcp;
pub mod memfiles;
pub mod models;
pub mod restore;
pub mod recall_status;
pub mod project_review;
pub mod setup;
pub mod snapshots;
pub mod update;
pub mod trash;
pub mod vault;

use std::path::{Path, PathBuf};

/// Background utilities must not create a console window in the desktop app.
pub fn background_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.stdin(std::process::Stdio::null());
    command
}

/// Data home: `$YOUMEM_HOME` or `~/.yourmem`.
pub fn data_home() -> PathBuf {
    if let Ok(p) = std::env::var("YOUMEM_HOME") {
        return PathBuf::from(p);
    }
    home_dir().join(".yourmem")
}

/// HOME 缺失属运行环境配置错误（launchd/守护进程边界）：fail-loud 好过静默把
/// 数据写进错误位置。显式旗标/`YOUMEM_HOME` 不经过这里，不受影响。
/// Windows 无 HOME 概念，回退链 USERPROFILE → HOME → HOMEDRIVE+HOMEPATH；
/// HOME 在 Windows 只于 MSYS/Cygwin 类环境出现，值可能是 POSIX 风格
/// （`/c/Users/...`，Windows API 会错解成当前盘符根下的 `\c\...`），所以只能
/// 排在 USERPROFILE 之后作回退。Unix 保持 HOME 最优先（macOS/Linux 语义不变）。
pub fn home_dir() -> PathBuf {
    resolve_home(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
        .expect("HOME/USERPROFILE 均未设置（守护进程上下文？设 HOME 或用 YOUMEM_HOME/显式目录旗标）")
}

/// HOME → USERPROFILE → HOMEDRIVE+HOMEPATH 回退链（Windows 上 USERPROFILE 最先，
/// 见 home_dir 注释），抽成纯函数便于测试各分支。
fn resolve_home(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        if let Some(up) = get("USERPROFILE") {
            return Some(PathBuf::from(up));
        }
    }
    if let Some(h) = get("HOME") {
        return Some(PathBuf::from(h));
    }
    if let Some(up) = get("USERPROFILE") {
        return Some(PathBuf::from(up));
    }
    match (get("HOMEDRIVE"), get("HOMEPATH")) {
        (Some(d), Some(p)) => Some(PathBuf::from(format!("{d}{p}"))),
        _ => None,
    }
}

/// 首次启动判定：数据目录里既无 config.json 也无 yourmem.db 才算新装——
/// 升级用户/已有数据的目录不受向导打扰。桌面端须在首个页面打开数据库
/// 之前调用（页面渲染即建库，之后此判定恒为 false）。
pub fn is_first_run(home: &Path) -> bool {
    !home.join("config.json").exists() && !home.join("yourmem.db").exists()
}

/// 备份根目录：config.json 的 `backup_dir`（支持 ~ 展开）优先，缺省/相对路径
/// 回落 `<数据目录>/backups`（历史行为）。DB 快照、purge 档案、会话导出、
/// 存储统计都从这里取根——新增备份落盘点必须走这个函数，别再 home.join("backups")。
/// 只挪备份，不动数据目录本体；位置变更由 backup_location 在迁移完成后写入。
pub fn backups_dir(home: &Path) -> PathBuf {
    let cfg = crate::ingest::read_config(home);
    let configured = cfg["backup_dir"].as_str().unwrap_or("").trim();
    if configured.is_empty() {
        return home.join("backups");
    }
    match PathBuf::from(expand_home(configured)) {
        p if p.is_absolute() => p,
        _ => home.join("backups"),
    }
}

/// 用户输入路径的 `~` / `~/` / `~\` 前缀展开（项目手动登记等入口；`~\`
/// 兼容 Windows 用户习惯）。
/// 不做 canonicalize——agent 记录的 cwd 是用户可见路径，符号链接归一反而
/// 会造出永远匹配不上会话的项目行。
pub fn expand_home(path: &str) -> String {
    if path == "~" {
        return home_dir().to_string_lossy().to_string();
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return home_dir().join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

pub fn default_claude_root() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

pub fn default_codex_root() -> PathBuf {
    home_dir().join(".codex").join("sessions")
}

pub fn default_zcode_root() -> PathBuf {
    home_dir().join(".zcode").join("cli").join("rollout")
}

pub fn default_kimi_root() -> PathBuf {
    home_dir().join(".kimi-code").join("sessions")
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolver(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn resolve_home_prefers_home() {
        let r = resolve_home(resolver(&[("HOME", "/u/a"), ("USERPROFILE", "C:\\Users\\a")]));
        assert_eq!(r, Some(PathBuf::from("/u/a")));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn resolve_home_windows_prefers_userprofile() {
        // MSYS/Git Bash 的 HOME 可能是 POSIX 风格（/c/...），Windows 侧必须让
        // 总是合法的 USERPROFILE 赢——HOME 只作 HOME 缺席时的回退。
        let r = resolve_home(resolver(&[("HOME", "/c/Users/a"), ("USERPROFILE", "C:\\Users\\a")]));
        assert_eq!(r, Some(PathBuf::from("C:\\Users\\a")));
        let r = resolve_home(resolver(&[("HOME", "/c/Users/a")]));
        assert_eq!(r, Some(PathBuf::from("/c/Users/a")));
    }

    #[test]
    fn resolve_home_falls_back_to_userprofile() {
        let r = resolve_home(resolver(&[("USERPROFILE", "C:\\Users\\a")]));
        assert_eq!(r, Some(PathBuf::from("C:\\Users\\a")));
    }

    #[test]
    fn resolve_home_falls_back_to_homedrive_homepath() {
        let r = resolve_home(resolver(&[("HOMEDRIVE", "C:"), ("HOMEPATH", "\\Users\\a")]));
        assert_eq!(r, Some(PathBuf::from("C:\\Users\\a")));
    }

    #[test]
    fn resolve_home_none_when_all_missing() {
        assert_eq!(resolve_home(resolver(&[])), None);
        // HOMEDRIVE/HOMEPATH 缺一半也不算
        assert_eq!(resolve_home(resolver(&[("HOMEDRIVE", "C:")])), None);
    }

    #[test]
    fn is_first_run_only_on_fresh_home() {
        let home = tempfile::tempdir().unwrap();
        assert!(is_first_run(home.path()), "全新目录算首次");
        std::fs::write(home.path().join("yourmem.db"), b"").unwrap();
        assert!(!is_first_run(home.path()), "已有数据库不算首次");
        let home2 = tempfile::tempdir().unwrap();
        std::fs::write(home2.path().join("config.json"), "{}").unwrap();
        assert!(!is_first_run(home2.path()), "已有配置不算首次");
    }

    #[test]
    fn backups_dir_defaults_to_data_dir_and_honors_config() {
        let home = tempfile::tempdir().unwrap();
        // 缺省（config 缺失/无该键/空值）：数据目录下的 backups
        assert_eq!(backups_dir(home.path()), home.path().join("backups"));
        // config 指定绝对路径后生效
        let ext = tempfile::tempdir().unwrap();
        crate::ingest::write_config(
            home.path(),
            &serde_json::json!({ "backup_dir": ext.path().to_string_lossy() }),
        )
        .unwrap();
        assert_eq!(backups_dir(home.path()), ext.path());
        // 相对路径视为无效配置，仍回落（用户手填错也不把备份写进工作目录）
        crate::ingest::write_config(home.path(), &serde_json::json!({ "backup_dir": "rel/path" }))
            .unwrap();
        assert_eq!(backups_dir(home.path()), home.path().join("backups"));
    }

    #[test]
    fn expand_home_tilde_variants() {
        let home = home_dir();
        assert_eq!(expand_home("~"), home.to_string_lossy());
        assert_eq!(expand_home("~/x/y"), home.join("x/y").to_string_lossy());
        // Windows 风格前缀同样展开（rest 内的 \ 由各平台 Path 处理）
        assert_eq!(expand_home("~\\x"), home.join("x").to_string_lossy());
        assert_eq!(expand_home("/abs/path"), "/abs/path");
        assert_eq!(expand_home("relative"), "relative");
    }
}
