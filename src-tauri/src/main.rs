//! yourmem desktop: thin Tauri shell over the yourmem core library.
//! Every command opens the DB read-fresh (WAL makes this cheap) and returns JSON.
//
// release 构建标记为 GUI 子系统：不带这行 exe 是 console 程序，用户启动
// app 会一直挂着黑色终端窗口（真机 2026-09-04 用户反馈）。debug 保留
// console——eprintln 的启动日志（auto-purge 等）看得见。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::{json, Value};
use tauri::Manager;
use yourmem::{data_home, db};

mod tray;

fn open() -> Result<rusqlite::Connection, String> {
    db::open(&data_home()).map_err(|e| e.to_string())
}

#[tauri::command]
fn stats() -> Result<Value, String> {
    db::stats(&open()?).map_err(|e| e.to_string())
}

#[tauri::command]
fn today() -> Result<Value, String> {
    let conn = open()?;
    let sessions = db::recent_sessions(&conn, None, 200).map_err(|e| e.to_string())?;
    // "今天"按本地日的 UTC 区间算（与日报卡同口径）；时间戳存 UTC，直接前缀匹配
    // 会让东八区 00:00-08:00 的会话算错天。
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let (lo, hi) = yourmem::dossier::day_bounds_utc(&today).map_err(|e| e.to_string())?;
    let in_day = |t: Option<&str>| t.map(|t| t >= lo.as_str() && t < hi.as_str()).unwrap_or(false);
    let todays: Vec<Value> = sessions
        .into_iter()
        .filter(|s| in_day(s["started_at"].as_str()) || in_day(s["ended_at"].as_str()))
        .collect();
    let tasks = db::open_tasks(&conn, None).map_err(|e| e.to_string())?;
    let recent = db::recent_sessions(&conn, None, 10).map_err(|e| e.to_string())?;
    // latest handoff per most-recent projects
    let projects = db::list_projects(&conn).map_err(|e| e.to_string())?;
    let mut handoffs = Vec::new();
    for p in projects.iter().take(3) {
        if let Some(h) = db::latest_handoff(&conn, p["id"].as_i64().unwrap_or(0)).map_err(|e| e.to_string())? {
            handoffs.push(json!({ "project": p["name"], "handoff": h }));
        }
    }
    Ok(json!({
        "today_sessions": todays,
        "open_tasks": tasks,
        "recent_sessions": recent,
        "recent_handoffs": handoffs,
        "stats": db::stats(&conn).map_err(|e| e.to_string())?,
    }))
}

#[tauri::command]
fn projects() -> Result<Value, String> {
    let conn = open()?;
    let mut rows = db::list_projects(&conn).map_err(|e| e.to_string())?;
    let mut archived = db::list_archived_projects(&conn).map_err(|e| e.to_string())?;
    // 展示层瘦身：$HOME 前缀对每行都是冗余宽度，换 ~（悬停 title 仍给全路径）。
    // 用 home_dir() 吃回退链（Windows 上 HOME 可能缺或为 MSYS 的 POSIX 风格），
    // 两种分隔符都认——Windows 项目路径是反斜杠。
    let home = yourmem::home_dir().to_string_lossy().to_string();
    let with_display = |rows: &mut Vec<Value>| {
        for p in rows.iter_mut() {
            if let Some(path) = p["path"].as_str() {
                let display = match path.strip_prefix(&home) {
                    Some("") => "~".to_string(),
                    Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => {
                        format!("~{rest}")
                    }
                    _ => path.to_string(),
                };
                p["display_path"] = json!(display);
            }
        }
    };
    with_display(&mut rows);
    with_display(&mut archived);
    Ok(json!({ "projects": rows, "archived": archived }))
}

/// 登记项目文件夹：~ 展开 + 真实存在校验；重复幂等、重登记归档项目即恢复。
#[tauri::command]
fn project_add(path: String) -> Result<Value, String> {
    let p = yourmem::expand_home(path.trim());
    if !std::path::Path::new(&p).is_dir() {
        return Err(format!("目录不存在：{p}"));
    }
    let conn = open()?;
    let (id, created) = db::add_project(&conn, &p).map_err(|e| e.to_string())?;
    Ok(json!({ "id": id, "created": created, "path": p }))
}

#[tauri::command]
fn project_archive(id: i64) -> Result<Value, String> {
    db::set_project_archived(&open()?, id, true).map_err(|e| e.to_string())?;
    Ok(json!({ "id": id, "archived": true }))
}

#[tauri::command]
fn project_restore(id: i64) -> Result<Value, String> {
    db::set_project_archived(&open()?, id, false).map_err(|e| e.to_string())?;
    Ok(json!({ "id": id, "archived": false }))
}

/// 在系统文件管理器中打开路径（右键菜单）：reveal=true 定位文件本身，false 打开目录。
/// macOS 用 open（reveal 加 -R）；Windows 用 explorer（/select 逗号分隔无空格）；
/// Linux 的 xdg-open 无 reveal 能力，退化为打开所在目录。
#[tauri::command]
fn open_in_finder(path: String, reveal: Option<bool>) -> Result<Value, String> {
    let p = yourmem::expand_home(path.trim());
    if !std::path::Path::new(&p).exists() {
        return Err(format!("路径不存在：{p}"));
    }
    let reveal = reveal.unwrap_or(false);
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("explorer");
        if reveal {
            c.arg(format!("/select,{p}"));
        } else {
            c.arg(&p);
        }
        c
    } else if cfg!(target_os = "linux") {
        let mut c = std::process::Command::new("xdg-open");
        if reveal {
            c.arg(std::path::Path::new(&p).parent().unwrap_or(std::path::Path::new(&p)));
        } else {
            c.arg(&p);
        }
        c
    } else {
        let mut c = std::process::Command::new("open");
        if reveal {
            c.arg("-R");
        }
        c.arg(&p);
        c
    };
    cmd.spawn().map_err(|e| format!("打开文件管理器失败：{e}"))?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
fn context(project_id: i64) -> Result<Value, String> {
    db::project_context(&open()?, project_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn project_dossier(project_id: i64) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "dossier");
    yourmem::dossier::project_dossier(&conn, project_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn daily_digest(day: Option<String>) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "digest");
    let day = day.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    yourmem::dossier::daily_digest(&conn, &day).map_err(|e| e.to_string())
}

// ------------------------------------------------ 可选 AI 整理（API key 只进系统凭据库）

const AI_KEY_SERVICE: &str = "yourmem.ai";
const AI_KEY_ACCOUNT: &str = "default";

fn ai_key_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(AI_KEY_SERVICE, AI_KEY_ACCOUNT)
        .map_err(|e| format!("无法访问系统凭据库：{e}"))
}

fn ai_key_from_store() -> Result<Option<(String, &'static str)>, String> {
    if let Ok(key) = std::env::var("YOUMEM_AI_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(Some((key, "environment")));
        }
    }
    match ai_key_entry()?.get_password() {
        Ok(key) if !key.trim().is_empty() => Ok(Some((key, "keyring"))),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("读取系统凭据库失败：{e}")),
    }
}

#[tauri::command]
fn ai_settings_get() -> Result<Value, String> {
    let mut out = yourmem::organizer::settings_json(&data_home());
    let key = ai_key_from_store()?;
    out["key_configured"] = json!(key.is_some());
    out["key_source"] = json!(key.map(|(_, source)| source));
    let s = yourmem::organizer::load_settings(&data_home());
    out["configured"] = json!(!s.model.trim().is_empty() && out["key_configured"] == true);
    Ok(out)
}

#[tauri::command]
fn ai_settings_save(
    base_url: String,
    model: String,
    max_input_chars: usize,
    api_key: Option<String>,
    clear_key: Option<bool>,
) -> Result<Value, String> {
    let settings = yourmem::organizer::AiSettings { base_url, model, max_input_chars };
    yourmem::organizer::validate_settings(&settings).map_err(|e| e.to_string())?;
    if clear_key.unwrap_or(false) {
        match ai_key_entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(format!("移除 API Key 失败：{e}")),
        }
    } else if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        ai_key_entry()?.set_password(key.trim())
            .map_err(|e| format!("保存 API Key 失败：{e}"))?;
    }
    yourmem::organizer::save_settings(&data_home(), &settings).map_err(|e| e.to_string())?;
    ai_settings_get()
}

#[tauri::command]
async fn ai_organize_day(day: Option<String>) -> Result<Value, String> {
    let day = day.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    run_blocking(move || {
        let home = data_home();
        let settings = yourmem::organizer::load_settings(&home);
        let (key, _) = ai_key_from_store()?.ok_or_else(|| "尚未配置 API Key".to_string())?;
        let conn = db::open(&home).map_err(|e| e.to_string())?;
        let activity = yourmem::dossier::daily_digest(&conn, &day).map_err(|e| e.to_string())?;
        let mut out = yourmem::organizer::organize_with_api(&settings, &key, &activity)
            .map_err(|e| e.to_string())?;
        out["day"] = json!(day);
        let _ = db::log_usage(&conn, "app", "ai_organize_day");
        Ok(out)
    }).await
}

#[tauri::command]
fn ai_summary_save(day: String, summary: Value) -> Result<Value, String> {
    let conn = open()?;
    let out = yourmem::organizer::save_project_summary(&conn, &day, &summary)
        .map_err(|e| e.to_string())?;
    let _ = db::log_usage(&conn, "app", "ai_summary_save");
    Ok(out)
}

#[tauri::command]
fn sessions(project_id: Option<i64>, limit: Option<u32>) -> Result<Value, String> {
    let conn = open()?;
    // limit 缺省 = 不限量（u32::MAX 对 SQLite 即无限）：对话资产不封顶，
    // 渲染分批由前端负责
    Ok(json!({ "sessions": db::recent_sessions(&conn, project_id, limit.unwrap_or(u32::MAX)).map_err(|e| e.to_string())? }))
}

#[tauri::command]
fn session(session_id: String, max: Option<u32>, line: Option<i64>, before_compact: Option<bool>) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "session");
    db::read_session(&conn, &session_id, max.unwrap_or(500), line, before_compact.unwrap_or(false))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn session_window(session_id: String, line: Option<i64>, offset: Option<u32>, message_id: Option<i64>, before_compact: Option<bool>) -> Result<Value, String> {
    db::session_window(&open()?, &session_id, line, offset, message_id, before_compact.unwrap_or(false)).map_err(|e| e.to_string())
}

#[tauri::command]
fn message_content(session_id: String, message_id: i64) -> Result<Value, String> {
    db::message_content(&open()?, &session_id, message_id).map_err(|e| e.to_string())
}

// ------------------------------------- 资产证明卡（UI-DESIGN §3，0.4.1）

fn session_agent_path(conn: &rusqlite::Connection, session_id: &str) -> Result<(String, String), String> {
    conn.query_row(
        "SELECT agent, file_path FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(|e| format!("session not found: {session_id} ({e})"))
}

/// 证明卡数据：vault 清单统计 + 源文件是否还在磁盘上（没了正好凸显备份价值）。
#[tauri::command]
fn session_proof(session_id: String) -> Result<Value, String> {
    let home = data_home();
    let conn = open()?;
    let (agent, file_path) = session_agent_path(&conn, &session_id)?;
    let mut p = yourmem::vault::session_proof(&conn, &home, &session_id).map_err(|e| e.to_string())?;
    p["agent"] = json!(agent);
    p["file_path"] = json!(file_path);
    p["source_exists"] = json!(std::path::Path::new(&file_path).is_file());
    // 写回仅限文件型 agent（restore::FILE_BASED_AGENTS）——SQLite 型不给入口
    p["writeback_supported"] = json!(yourmem::restore::FILE_BASED_AGENTS.contains(&agent.as_str()));
    Ok(p)
}

/// 立即校验：逐对象重算哈希（vault::verify_session）。
#[tauri::command]
async fn session_verify(session_id: String) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "session verify");
        yourmem::vault::verify_session(&conn, &home, &session_id).map_err(|e| e.to_string())
    })
    .await
}

/// 导出原件：字节级重建到 home/backups/export/（逐对象哈希校验随 export 自带）。
/// 零构建 UI 没有文件对话框，落固定目录并把路径交给用户复制。
#[tauri::command]
async fn session_export(session_id: String) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "session export");
        let out = yourmem::backups_dir(&home)
            .join("export")
            .join(format!("{}.jsonl", session_id.replace(':', "_")));
        let lines = yourmem::vault::export_session(&conn, &home, &session_id, &out)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "path": out, "lines": lines }))
    })
    .await
}

/// 写回预览（门控要素 1）：restore::plan 的路径清单与后果。
#[tauri::command]
fn session_writeback_plan(session_id: String) -> Result<Value, String> {
    let conn = open()?;
    let (agent, _) = session_agent_path(&conn, &session_id)?;
    yourmem::restore::plan(&conn, &agent, &session_id).map_err(|e| e.to_string())
}

/// 写回执行：确认由前端 armed 按钮承担（门控语义不变：预览→确认→.bak→执行）。
/// force=true 对应 CLI --force（目标已存在时先 .bak 再覆盖）。
#[tauri::command]
async fn session_writeback(session_id: String, force: bool) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "session writeback");
        let (agent, _) = session_agent_path(&conn, &session_id)?;
        yourmem::restore::execute(&conn, &home, &agent, &session_id, force).map_err(|e| e.to_string())
    })
    .await
}

/// 能力矩阵（UI-DESIGN §8.4）：adapter 六格诚实自报 + 加密硬阻断观察项。
#[tauri::command]
fn capability_matrix() -> Result<Value, String> {
    Ok(json!({
        "adapters": yourmem::adapters::capability_matrix(),
        "encrypted_watchlist": yourmem::adapters::encrypted_watchlist(),
    }))
}

// 软删回收站（DESIGN-0.3 §6）：只翻动 deleted_at；删除/恢复的确认由前端二次确认按钮承担。
#[tauri::command]
fn session_delete(session_id: String) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "session delete");
    db::set_session_deleted(&conn, &session_id, true).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
fn session_restore(session_id: String) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "session restore");
    db::set_session_deleted(&conn, &session_id, false).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
fn trash_list() -> Result<Value, String> {
    Ok(json!({ "trash": db::trash_sessions(&open()?).map_err(|e| e.to_string())? }))
}

/// 彻底删除单条回收站会话（归档式 purge：保留期校验 + 锁内备份后执行）。
/// 未超期的会话会被 db 层拒绝（错误信息含剩余天数），前端据返回文案提示。
/// 彻底删除单条回收站会话（1.0.0 用户裁定：彻底删除 = 原件一并移除，不再
/// 保留离线档案——"都提醒了那就是彻底删了"；CLI 如需留档走 --keep-archive）。
/// force=true 旁路 30 天保留期。
#[tauri::command]
async fn trash_purge(session_id: String, force: Option<bool>) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let mut conn = db::open(&home).map_err(|e| e.to_string())?;
        yourmem::trash::purge_desktop(&mut conn, &home, &session_id, force.unwrap_or(false)).map_err(|e| e.to_string())
    })
    .await
}

/// 批量彻底删除选中的回收站会话：一次锁内逐条 purge，单条失败不中断整批，
/// 逐条回报。语义同 trash_purge（不保留离线档案）。
#[tauri::command]
async fn trash_purge_selected(ids: Vec<String>, force: Option<bool>) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let mut conn = db::open(&home).map_err(|e| e.to_string())?;
        yourmem::trash::purge_selected(&mut conn, &home, &ids, force.unwrap_or(false)).map_err(|e| e.to_string())
    })
    .await
}

/// 清空回收站超期项（超过 TRASH_RETENTION_DAYS=30 天）。执行集合 = 当刻超期
/// 集合（与 CLI session empty 同一预览→执行语义，确认由前端 armed 按钮承担）。
#[tauri::command]
async fn trash_empty_overdue() -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let mut conn = db::open(&home).map_err(|e| e.to_string())?;
        yourmem::trash::empty_desktop(&mut conn, &home).map_err(|e| e.to_string())
    })
    .await
}

/// 回收站自动清理开关（config.json auto_purge_trash，默认关）。
/// 开启后每次 app 启动静默清一次超期项——归档式 purge（对象进 backups/purge/），
/// 不是 unlink；自动的只是"超期即清"这个动作，备份与门控语义不变。
#[tauri::command]
fn auto_purge_get() -> Result<Value, String> {
    let cfg = yourmem::ingest::read_config(&data_home());
    Ok(json!({ "auto_purge_trash": cfg["auto_purge_trash"].as_bool().unwrap_or(false) }))
}

#[tauri::command]
fn auto_purge_set(enabled: bool) -> Result<Value, String> {
    let home = data_home();
    let mut cfg = yourmem::ingest::read_config(&home);
    cfg["auto_purge_trash"] = json!(enabled);
    yourmem::ingest::write_config(&home, &cfg).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "auto_purge_trash": enabled }))
}

/// 首次启动状态：新装（数据目录无 config.json 也无 yourmem.db）时前端弹
/// 安装向导选备份位置。UI 在首屏渲染前调用（页面渲染即建库）。
/// suggested 是备份位置预填建议：存在非系统盘时建议 `<盘>:\yourmem-backup`
/// （用户装 app 常挑非 C 盘，备份同样不该默认堆 C 盘）。
#[tauri::command]
fn first_run_state() -> Result<Value, String> {
    let home = data_home();
    let first_run = yourmem::is_first_run(&home);
    let suggested = if first_run { suggest_backup_dir() } else { None };
    Ok(json!({
        "first_run": first_run,
        "home": home,
        "backup_dir": yourmem::backups_dir(&home),
        "suggested": suggested,
    }))
}

/// 向导预填建议：`fsutil fsinfo drives` 枚举盘符（系统内置、瞬时、无权限要求），
/// 取第一个非系统盘。**不做任何盘上 IO**——死掉的映射网络盘 metadata 会卡住
/// 调用，枚举盘符本身不会；fsutil 失败/只有一块盘/非 Windows 一律 None。
/// 输出前缀随系统语言本地化（"Drives:"/"驱动器:"），解析按 token 形状匹配、
/// 不认标签。注意子命令是 fsinfo——`fsutil fs drives` 在部分系统无效。
fn suggest_backup_dir() -> Option<String> {
    if !cfg!(target_os = "windows") {
        return None;
    }
    let out = yourmem::background_command("fsutil")
        .args(["fsinfo", "drives"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let letters = parse_drive_letters(&String::from_utf8_lossy(&out.stdout));
    let system = std::env::var("SystemDrive")
        .unwrap_or_else(|_| "C:".into())
        .to_uppercase();
    pick_suggestion(&letters, &system)
}

/// 解析 fsutil 输出（形如 "Drives: C:\ D:\ E:\"）里的盘符 token。
fn parse_drive_letters(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|t| t.len() == 3 && t.as_bytes()[1] == b':' && t.ends_with('\\'))
        .map(|t| t[..2].to_uppercase())
        .collect()
}

/// 取第一个非系统盘的备份目录建议（盘符比较大小写不敏感，输出归一为大写）。
fn pick_suggestion(letters: &[String], system: &str) -> Option<String> {
    let system = system.to_uppercase();
    letters
        .iter()
        .find(|l| l.to_uppercase() != system)
        .map(|l| format!("{}\\yourmem-backup", l.to_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drive_letters_from_fsutil_output() {
        assert_eq!(
            parse_drive_letters("Drives: C:\\ D:\\ E:\\\r\n"),
            vec!["C:", "D:", "E:"]
        );
        assert_eq!(parse_drive_letters("Drives: C:\\"), vec!["C:"]);
        assert!(parse_drive_letters("garbage").is_empty());
    }

    #[test]
    fn pick_suggestion_skips_system_drive() {
        let letters = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            pick_suggestion(&letters(&["C:", "D:", "E:"]), "C:"),
            Some("D:\\yourmem-backup".into())
        );
        assert_eq!(pick_suggestion(&letters(&["C:"]), "C:"), None, "只有系统盘不建议");
        assert_eq!(
            pick_suggestion(&letters(&["C:", "c:", "D:"]), "C:"),
            Some("D:\\yourmem-backup".into()),
            "盘符大小写不敏感"
        );
    }
}

/// 检查更新：当前版本 vs GitHub 最新 release（私有仓库走本机 gh CLI）。
#[tauri::command]
async fn update_check() -> Result<Value, String> {
    run_blocking(move || {
        let current = env!("CARGO_PKG_VERSION");
        let latest = yourmem::update::latest_release().map_err(|e| e.to_string())?;
        let tag = latest["tag"].as_str().unwrap_or("").trim().to_string();
        let clean = tag.trim_start_matches(['v', 'V']).to_string();
        Ok(json!({
            "current": current,
            "latest": clean,
            "url": latest["url"],
            "published_at": latest["published_at"],
            "update_available": yourmem::update::version_newer(&tag, current),
        }))
    })
    .await
}

/// 系统默认浏览器打开外链（只放行 http/https 且无空白，防参数注入）。
#[tauri::command]
fn open_url(url: String) -> Result<Value, String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.contains(char::is_whitespace)
    {
        return Err(format!("只支持打开 http(s) 链接"));
    }
    if cfg!(target_os = "windows") {
        std::process::Command::new("explorer")
            .arg(url)
            .spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(url).spawn()
    } else {
        std::process::Command::new("open").arg(url).spawn()
    }
    .map_err(|e| format!("打开浏览器失败：{e}"))?;
    Ok(json!({ "ok": true }))
}

/// 备份位置（config.json backup_dir，落点统一走 yourmem::backups_dir）。
/// configured 是用户填的原文（空 = 未设置），effective 是当前生效路径。
#[tauri::command]
async fn backup_dir_pick(window: tauri::WebviewWindow, path: String) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt;
    run_blocking(move || {
        let mut dialog = window.dialog().file().set_title("选择文件夹").set_parent(&window);
        let initial = std::path::PathBuf::from(yourmem::expand_home(path.trim()));
        if initial.is_absolute() && initial.is_dir() {
            dialog = dialog.set_directory(initial);
        }
        dialog.blocking_pick_folder()
            .map(|p| p.into_path().map(|p| p.to_string_lossy().into_owned()).map_err(|e| e.to_string()))
            .transpose().map(|path| json!(path))
    }).await
}

#[tauri::command]
fn backup_dir_get() -> Result<Value, String> {
    let home = data_home();
    let configured = yourmem::ingest::read_config(&home)["backup_dir"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let effective = yourmem::backups_dir(&home);
    Ok(json!({ "configured": configured, "effective": effective }))
}

#[tauri::command]
async fn bundle_path_pick(window: tauri::WebviewWindow, path: String, save: bool) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt;
    run_blocking(move || {
        let mut dialog = window.dialog().file().set_parent(&window)
            .set_title(if save { "选择备份保存位置" } else { "选择完整备份" })
            .add_filter("yourmem 备份", &["gz"]);
        let initial = std::path::PathBuf::from(yourmem::expand_home(path.trim()));
        if let Some(parent) = initial.parent().filter(|p| p.is_absolute() && p.is_dir()) {
            dialog = dialog.set_directory(parent);
        }
        if save {
            if let Some(name) = initial.file_name() { dialog = dialog.set_file_name(name.to_string_lossy()); }
        }
        let selected = if save { dialog.blocking_save_file() } else { dialog.blocking_pick_file() };
        selected.map(|p| p.into_path().map_err(|e| e.to_string()))
            .transpose().map(|p| json!(p))
    }).await
}

/// 设置备份位置：空串 = 回到默认（数据目录下的 backups）。已有备份先迁移并
/// 校验，成功后才切换 config；目标非空时拒绝，避免覆盖已有文件。
#[tauri::command]
async fn backup_dir_set(path: String) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        yourmem::backup_location::set(&home, &path).map_err(|e| format!("{e:#}"))
    })
    .await
}

/// 启动时若开关开启则静默清一次超期项（尽力而为，失败只记 stderr）。
fn auto_purge_if_enabled() {
    let home = data_home();
    if !yourmem::ingest::read_config(&home)["auto_purge_trash"].as_bool().unwrap_or(false) {
        return;
    }
    let r = (|| -> anyhow::Result<()> {
        let mut conn = db::open(&home)?;
        let r = yourmem::trash::empty_on_startup(&mut conn, &home)?;
        let n = r["purged_sessions"].as_u64().unwrap_or(0);
        if n > 0 { eprintln!("yourmem app: 自动清理了 {} 个超期回收站会话", n); }
        Ok(())
    })();
    if let Err(e) = r {
        eprintln!("yourmem app: auto-purge 失败（不影响启动）: {e:#}");
    }
}

#[tauri::command]
fn search(query: String, project: Option<String>, agent: Option<String>, kind: Option<String>, limit: Option<u32>) -> Result<Value, String> {
    let result = (|| {
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "search");
        db::search(&conn, &db::SearchOpts {
            query,
            project,
            agent,
            kind,
            limit: limit.unwrap_or(50),
        })
        .map_err(|e| e.to_string())
    })();
    yourmem::recall_status::record(&data_home(), "app", "search", result.is_ok(), result.as_ref().ok().map(|v| v.len() as u64));
    Ok(json!({ "results": result? }))
}

#[tauri::command]
fn memories(project: Option<String>, status: Option<String>, r#type: Option<String>, scope: Option<String>, agent: Option<String>, limit: Option<u32>) -> Result<Value, String> {
    let conn = open()?;
    let pid = match &project {
        Some(p) => db::resolve_project(&conn, Some(p), None).map_err(|e| e.to_string())?.map(|x| x.0),
        None => None,
    };
    let mems = db::list_memories(&conn, &db::MemoryFilter {
        project_id: pid,
        scope,
        r#type,
        status,
        agent,
        include_global: pid.is_some(),
        limit: limit.unwrap_or(200),
    })
    .map_err(|e| e.to_string())?;
    Ok(json!({ "memories": mems }))
}

// -------------------------------------------------- agent 数据源（设置页）

#[tauri::command]
fn agents_detect() -> Result<Value, String> {
    yourmem::ingest::agent_sources(&data_home(), &open()?).map_err(|e| e.to_string())
}

#[tauri::command]
fn agent_add_root(agent: String, path: String) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "agent_add_root");
    yourmem::ingest::add_extra_root(&data_home(), &agent, std::path::Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn agent_remove_root(agent: String, path: String) -> Result<Value, String> {
    let conn = open()?;
    let _ = db::log_usage(&conn, "app", "agent_remove_root");
    yourmem::ingest::remove_extra_root(&data_home(), &agent, std::path::Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn update_memory(id: String, action: String, superseded_by: Option<String>) -> Result<Value, String> {
    db::update_memory_status(&open()?, &id, &action, superseded_by.as_deref()).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
fn artifacts(project_id: Option<i64>, session_id: Option<String>, limit: Option<u32>) -> Result<Value, String> {
    let conn = open()?;
    Ok(json!({ "artifacts": db::list_artifacts(&conn, project_id, session_id.as_deref(), limit.unwrap_or(200)).map_err(|e| e.to_string())? }))
}

#[tauri::command]
async fn import_now() -> Result<Value, String> {
    // Tauri 2 的同步命令在主线程执行——大库首次导入以分钟计，主线程被占满
    // 就是整窗冻结（真机 2026-09-03：点采集按钮 UI 卡死）。耗时命令一律
    // spawn_blocking；前端 invoke 调用方式不变。
    run_blocking(|| {
        let home = data_home();
        let outcome = yourmem::ingest::import_defaults(&home).map_err(|e| e.to_string())?;
        if let Ok(conn) = yourmem::db::open(&home) {
            let _ = yourmem::db::log_usage(&conn, "app", "import_now");
        }
        Ok(yourmem::ingest::outcome_json(&outcome))
    })
    .await
}

/// 耗时命令统一丢到阻塞线程池：不占主线程，WebView 事件循环保持响应。
async fn run_blocking<F>(f: F) -> Result<Value, String>
where
    F: FnOnce() -> Result<Value, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("任务失败：{e}"))?
}

#[tauri::command]
fn memory_files() -> Result<Value, String> {
    Ok(json!({ "memory_files": db::list_memory_files(&open()?).map_err(|e| e.to_string())? }))
}

#[tauri::command]
fn memory_file_show(id: i64, revision: Option<i64>) -> Result<Value, String> {
    yourmem::memfiles::show(&open()?, &data_home(), id, revision).map_err(|e| e.to_string())
}

// -------------------------------------------------- 存储占用 / 索引范围（设置页）

#[tauri::command]
async fn index_status() -> Result<Value, String> {
    run_blocking(|| db::index_status(&open()?).map_err(|e| e.to_string())).await
}

/// 全量/轻量切换入口：开 = 重建工具输出索引（分钟级）；关 = 清索引 + VACUUM
/// 回收磁盘（分钟级）。数据层不动，可随时往复。
#[tauri::command]
async fn index_set_tools(full: bool) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "index tools");
        db::set_tool_index(&home, &conn, full).map_err(|e| e.to_string())
    })
    .await
}

/// 数据目录三项占用。objects 是几十万碎文件，遍历秒级——必须阻塞线程。
#[tauri::command]
async fn storage_usage() -> Result<Value, String> {
    run_blocking(|| Ok(db::storage_usage(&data_home()))).await
}

/// 整理数据库空闲页（VACUUM）：索引切换 / 大量删除后的磁盘回收。
#[tauri::command]
async fn compact_db() -> Result<Value, String> {
    run_blocking(|| {
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "index compact");
        conn.execute_batch("VACUUM").map_err(|e| e.to_string())?;
        db::index_status(&conn).map_err(|e| e.to_string())
    })
    .await
}

// ---------------------------------------------------------------- settings

#[tauri::command]
async fn bundle_create(out: String) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "bundle_create");
        yourmem::bundle::create(&conn, &home, std::path::Path::new(&out), &yourmem::bundle::BundleFilter::default())
            .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
async fn bundle_verify(path: String) -> Result<Value, String> {
    run_blocking(move || yourmem::bundle::restore_plan(std::path::Path::new(&path), &data_home()).map_err(|e| e.to_string())).await
}

/// 从 bundle 恢复到当前库（merge=true 合并；false 要求库不存在——UI 里常态是合并）。
#[tauri::command]
async fn bundle_restore(path: String, merge: bool) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let restored = yourmem::bundle::restore(std::path::Path::new(&path), &home, merge).map_err(|e| format!("{e:#}"))?;
        if let Ok(conn) = open() {
            let _ = db::log_usage(&conn, "app", "bundle_restore");
        }
        Ok(restored)
    })
    .await
}

#[tauri::command]
async fn setup_plan(agents: Vec<String>) -> Result<Value, String> {
    run_blocking(move || yourmem::setup::plan_selected(&yourmem::setup::Targets::default(), &agents).map_err(|e| e.to_string())).await
}

#[tauri::command]
async fn project_review(project_id: i64, mark_reviewed: Option<bool>) -> Result<Value, String> {
    run_blocking(move || {
        let conn = open()?;
        let home = data_home();
        if mark_reviewed.unwrap_or(false) { yourmem::project_review::mark_reviewed(&conn, &home, project_id) }
        else { yourmem::project_review::status(&conn, &home, project_id) }.map_err(|e| e.to_string())
    }).await
}

/// UI 侧的"确认"由前端二次确认按钮承担（门控语义不变：预览→确认→备份→执行）。
#[tauri::command]
async fn setup_run(agents: Vec<String>) -> Result<Value, String> {
    run_blocking(move || {
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "setup_run");
        yourmem::setup::execute_selected(&yourmem::setup::Targets::default(), &agents).map_err(|e| e.to_string())
    })
    .await
}

/// 本地自检（UI-DESIGN §8.5）：只读对账，报告直接渲染在设置页。
#[tauri::command]
async fn doctor() -> Result<Value, String> {
    run_blocking(|| {
        let home = data_home();
        let conn = open()?;
        let _ = db::log_usage(&conn, "app", "doctor");
        yourmem::doctor::run(&conn, &home).map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
fn app_info() -> Result<Value, String> {
    let home = data_home();
    let conn = open()?;
    let schema_version: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "schema_version": schema_version,
        "home": home,
        "recall_status": yourmem::recall_status::read(&home),
        "usage_last_7d": db::usage_summary(&conn, 7).map_err(|e| e.to_string())?,
    }))
}

/// 停用/启用一个内置 agent（停用后不采集其源；已入库历史保留可搜）。
#[tauri::command]
fn agent_set_enabled(agent: String, enabled: bool) -> Result<Value, String> {
    let home = data_home();
    yourmem::ingest::set_agent_disabled(&home, &agent, !enabled).map_err(|e| e.to_string())
}

// ------------------------------------------------ 彻底删除的离线档案管理

fn purge_archive_root() -> std::path::PathBuf {
    yourmem::backups_dir(&data_home()).join("purge")
}

fn dir_size(path: &std::path::Path) -> u64 {
    std::fs::read_dir(path).map(|rd| {
        rd.filter_map(|e| e.ok()).map(|e| {
            if e.path().is_dir() { dir_size(&e.path()) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }
        }).sum()
    }).unwrap_or(0)
}

/// 列出彻底删除留下的离线档案（backups/purge/ 下每个目录一条：名称/大小/时间）。
#[tauri::command]
fn purge_archives() -> Result<Value, String> {
    let root = purge_archive_root();
    let mut items: Vec<Value> = std::fs::read_dir(&root).map(|rd| {
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some(json!({
                    "name": e.file_name(),
                    "bytes": dir_size(&e.path()),
                    "modified": meta.modified().ok()
                        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))?,
                }))
            })
            .collect()
    }).unwrap_or_default();
    items.sort_by(|a, b| b["modified"].as_str().cmp(&a["modified"].as_str()));
    let total: u64 = items.iter().filter_map(|i| i["bytes"].as_u64()).sum();
    Ok(json!({ "archives": items, "total_bytes": total, "root": root }))
}

/// 删除单个档案目录（name 只取文件名成分防目录穿越）。
#[tauri::command]
fn purge_archive_delete(name: String) -> Result<Value, String> {
    let safe = std::path::Path::new(&name)
        .file_name()
        .ok_or_else(|| "非法档案名".to_string())?;
    let target = purge_archive_root().join(safe);
    std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": safe.to_string_lossy() }))
}

/// 清空全部离线档案（backups/purge/ 整目录内容移除，目录本身保留）。
#[tauri::command]
fn purge_archive_clear() -> Result<Value, String> {
    let root = purge_archive_root();
    let mut removed = 0usize;
    for e in std::fs::read_dir(&root).map_err(|e| e.to_string())?.filter_map(|e| e.ok()) {
        if e.path().is_dir() {
            std::fs::remove_dir_all(e.path()).map_err(|e| e.to_string())?;
            removed += 1;
        }
    }
    Ok(json!({ "removed": removed }))
}

#[tauri::command]
async fn snapshot_list() -> Result<Value, String> {
    run_blocking(|| yourmem::snapshots::list(&data_home()).map_err(|e| format!("{e:#}"))).await
}
#[tauri::command]
async fn snapshot_create() -> Result<Value, String> {
    run_blocking(|| yourmem::snapshots::create(&data_home()).map_err(|e| format!("{e:#}"))).await
}
#[tauri::command]
async fn snapshot_export(id: String) -> Result<Value, String> {
    run_blocking(move || {
        let home = data_home();
        let out = yourmem::backups_dir(&home).join("export").join(format!("snapshot-{id}.tar.gz"));
        yourmem::snapshots::export(&home, &id, &out).map_err(|e|format!("{e:#}"))
    }).await
}
#[tauri::command]
async fn snapshot_cleanup_plan(keep_recent: usize, keep_monthly: usize) -> Result<Value, String> {
    run_blocking(move || yourmem::snapshots::cleanup_plan(&data_home(),keep_recent,keep_monthly).map_err(|e|format!("{e:#}"))).await
}
#[tauri::command]
async fn snapshot_cleanup(keep_recent: usize, keep_monthly: usize, token: String) -> Result<Value, String> {
    run_blocking(move || yourmem::snapshots::cleanup(&data_home(),keep_recent,keep_monthly,&token).map_err(|e|format!("{e:#}"))).await
}

fn main() {
    // 参数路由先行：安装包主程序名与 CLI 同名（productName），无法也不必靠
    // 文件名区分身份——app 本体直接兼任 agent 端点。`mcp` 进 stdio 服务
    // （agent 每次拉起新进程，GUI 永不出现）；`--version` 打印即退（setup
    // 的 stale 探针依赖）。无参才启动窗口。
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") => {
            let home = data_home();
            if let Err(e) = yourmem::mcp::serve(&home) {
                eprintln!("yourmem mcp: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        Some("--version") | Some("version") => {
            println!("yourmem {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        _ => {}
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            tray::show_main(app);
        }))
        .setup(|app| {
            auto_purge_if_enabled();
            tray::setup(app)?;
            // Windows 任务栏/标题栏图标来自窗口 class,debug 构建不嵌入 exe 资源
            // （bundle 只在 tauri build 时打）——代码内显式设置,开发态也正常。
            if let Some(win) = app.get_webview_window("main") {
                // 128px 源：32px 位图被任务栏/Alt-Tab 上采样会发虚（真机反馈
                // "图标看着小一圈"），大图交给系统按需缩放
                if let Ok(img) = tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png")) {
                    let _ = win.set_icon(img);
                }
                // 深色标题栏（Win10 1809+ DWM；config theme 在部分 WebView2 版本
                // 上不生效，代码内再设一道双保险）
                let _ = win.set_theme(Some(tauri::Theme::Dark));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Only keep the process alive when the window was hidden successfully.
                    if window.hide().is_ok() {
                        api.prevent_close();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            session_window, message_content, stats, today, projects, context, project_dossier, daily_digest, sessions, session,
            ai_settings_get, ai_settings_save, ai_organize_day, ai_summary_save,
            project_add, project_archive, project_restore, open_in_finder,
            session_proof, session_verify, session_export, session_writeback_plan,
            session_writeback, capability_matrix,
            session_delete, session_restore, trash_list, trash_purge, trash_purge_selected,
            trash_empty_overdue, purge_archives, purge_archive_delete, purge_archive_clear,
            auto_purge_get, auto_purge_set, agent_set_enabled,
            backup_dir_get, backup_dir_set, backup_dir_pick, bundle_path_pick,
            first_run_state, update_check, open_url,
            search, memories, update_memory, artifacts, import_now,
            memory_files, memory_file_show,
            agents_detect, agent_add_root, agent_remove_root,
            bundle_create, bundle_verify, bundle_restore, project_review, setup_plan, setup_run, app_info,
            snapshot_list, snapshot_create, snapshot_export, snapshot_cleanup_plan, snapshot_cleanup,
            doctor,
            index_status, index_set_tools, storage_usage, compact_db,
        ])
        .run(tauri::generate_context!())
        .expect("error while running yourmem app");
}
