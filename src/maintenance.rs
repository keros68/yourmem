//! Background maintenance run by the desktop collection worker: periodic
//! incremental snapshots, a daily self-check and removal of stale object
//! temp files. Results live in `maintenance.json` so the UI only has to show
//! problems.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DOCTOR_INTERVAL_SECS: u64 = 86_400;
/// Off until snapshots stop copying every vault object as its own file: on a
/// real archive the first snapshot runs for hours while holding the import lock.
const DEFAULT_SNAPSHOT_INTERVAL_DAYS: u64 = 0;
const STALE_TMP: Duration = Duration::from_secs(86_400);

fn state_path(home: &Path) -> PathBuf {
    home.join("maintenance.json")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn due(last: &Value, interval: u64, now: u64) -> bool {
    last.as_u64().map_or(true, |t| now.saturating_sub(t) >= interval)
}

/// Last maintenance results; empty object before the first run.
pub fn status(home: &Path) -> Value {
    std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save(home: &Path, state: &Value) -> Result<()> {
    let tmp = home.join(format!("maintenance.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(&tmp, state_path(home))?;
    Ok(())
}

/// Snapshot interval in days from config.json `snapshot_interval_days`;
/// 0 turns automatic snapshots off.
fn snapshot_interval_days(home: &Path) -> u64 {
    crate::ingest::read_config(home)["snapshot_interval_days"]
        .as_u64()
        .unwrap_or(DEFAULT_SNAPSHOT_INTERVAL_DAYS)
}

/// Object temp files are renamed into place within milliseconds; one older
/// than a day was left by a crash.
fn remove_stale_tmp(home: &Path) -> usize {
    let root = crate::vault::objects_root(home);
    let mut removed = 0;
    for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() || !entry.file_name().to_string_lossy().contains(".tmp.") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= STALE_TMP);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Run whatever is due. Each task records its attempt time, so a failing task
/// is retried on its next interval rather than on every collection pass.
pub fn run_due(home: &Path) -> Result<Value> {
    let mut state = status(home);
    let now = now_secs();
    let mut ran = Vec::new();

    let interval_days = snapshot_interval_days(home);
    if interval_days > 0 && due(&state["snapshot_attempt_at"], interval_days * 86_400, now) {
        state["snapshot_attempt_at"] = json!(now);
        state["snapshot"] = match crate::snapshots::create(home) {
            Ok(r) => json!({"ok": true, "at": crate::now_iso(), "id": r["id"]}),
            Err(e) => json!({"ok": false, "at": crate::now_iso(), "error": format!("{e:#}")}),
        };
        save(home, &state)?;
        ran.push("snapshot");
    }

    if due(&state["doctor_attempt_at"], DOCTOR_INTERVAL_SECS, now) {
        state["doctor_attempt_at"] = json!(now);
        let tmp_removed = remove_stale_tmp(home);
        let report = crate::db::open(home).and_then(|conn| crate::doctor::run(&conn, home));
        state["doctor"] = match report {
            Ok(r) => {
                let problems: Vec<Value> = r["checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["status"] != "ok")
                    // 未开启自动快照时，"快照过旧"不是需要处理的问题
                    .filter(|c| interval_days > 0 || c["name"] != "db_snapshot")
                    .cloned()
                    .collect();
                json!({"ok": r["ok"], "at": r["checked_at"], "problems": problems, "tmp_removed": tmp_removed})
            }
            Err(e) => json!({
                "ok": false,
                "at": crate::now_iso(),
                "problems": [{"name": "doctor", "status": "fail", "detail": format!("{e:#}")}],
                "tmp_removed": tmp_removed,
            }),
        };
        save(home, &state)?;
        ran.push("doctor");
    }

    Ok(json!({"ran": ran}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_once_per_interval_and_records_results() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        crate::ingest::write_config(home, &json!({"snapshot_interval_days": 7})).unwrap();
        let stale = crate::vault::objects_root(home).join("ab");
        std::fs::create_dir_all(&stale).unwrap();
        let tmp = stale.join("abcd.tmp.1");
        std::fs::write(&tmp, b"x").unwrap();
        let old = SystemTime::now() - Duration::from_secs(2 * 86_400);
        std::fs::File::options().write(true).open(&tmp).unwrap().set_modified(old).unwrap();

        let first = run_due(home).unwrap();
        assert_eq!(first["ran"], json!(["snapshot", "doctor"]));
        let st = status(home);
        assert_eq!(st["snapshot"]["ok"], true, "{st}");
        assert_eq!(st["doctor"]["tmp_removed"], 1);
        assert!(!tmp.exists());
        assert!(
            st["doctor"]["problems"].as_array().unwrap().iter().all(|p| p["name"] != "db_snapshot"),
            "the automatic snapshot counts as fresh: {st}"
        );

        let second = run_due(home).unwrap();
        assert_eq!(second["ran"], json!([]));
    }

    #[test]
    fn snapshots_are_off_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        let r = run_due(home).unwrap();
        assert_eq!(r["ran"], json!(["doctor"]));
        let problems = status(home)["doctor"]["problems"].clone();
        assert!(problems.as_array().unwrap().iter().all(|p| p["name"] != "db_snapshot"), "{problems}");
    }
}
