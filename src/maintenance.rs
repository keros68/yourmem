//! Background maintenance run by the desktop collection worker: moving
//! legacy object files into the object store, periodic incremental snapshots,
//! a daily self-check and removal of stale object temp files. Results live in
//! `maintenance.json` so the UI only has to show problems.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DOCTOR_INTERVAL_SECS: u64 = 86_400;
/// Weekly: on a real archive of ~470k objects the first snapshot takes about
/// five minutes and later ones about two, with collection paused meanwhile.
const DEFAULT_SNAPSHOT_INTERVAL_DAYS: u64 = 7;
const STALE_TMP: Duration = Duration::from_secs(86_400);
const MIGRATE_BATCH: usize = 20_000;
/// The worker never waits long for a lock: someone is collecting or editing.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// A snapshot deferred by a held lock is retried after this many seconds.
const BUSY_RETRY_SECS: u64 = 600;

/// The database or repository was locked by another writer: not a failure.
pub(crate) fn is_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<rusqlite::Error>().and_then(|e| e.sqlite_error_code()).is_some_and(|code| {
            matches!(code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        })
    })
}

fn state_path(home: &Path) -> PathBuf {
    home.join("maintenance.json")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn due(last: &Value, interval: u64, now: u64) -> bool {
    last.as_u64().map_or(true, |t| now.saturating_sub(t) >= interval)
}

fn load(home: &Path) -> Value {
    let mut state: Value = std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    normalize(&mut state);
    state
}

/// Bring state written by 1.3.0 in line with current rules: a snapshot that
/// failed only because a lock was held counts as deferred and is retried at
/// once, and "no snapshot yet" is not a problem before the first success.
fn normalize(state: &mut Value) {
    let snap = &state["snapshot"];
    let lock_failure = snap["ok"] == false
        && snap["retry"].is_null()
        && snap["error"].as_str().is_some_and(|e| e.contains("locked") || e.contains("busy"));
    if lock_failure {
        state["snapshot"] = json!({"ok": false, "retry": true, "at": snap["at"].clone()});
        state["snapshot_attempt_at"] = Value::Null;
    }
    if state["snapshot"]["ok"] != true {
        if let Some(problems) = state["doctor"]["problems"].as_array_mut() {
            problems.retain(|p| p["name"] != "db_snapshot");
        }
    }
}

/// Last maintenance results plus the snapshot interval in effect.
pub fn status(home: &Path) -> Value {
    let mut state = load(home);
    state["snapshot_interval_days"] = json!(snapshot_interval_days(home));
    state
}

/// Serialises read-modify-write of `maintenance.json` between the background
/// worker and on-demand commands in the same process.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Re-read the state, apply `f` and write it back, so one writer never
/// overwrites fields another one changed meanwhile.
fn update(home: &Path, f: impl FnOnce(&mut Value)) -> Result<()> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load(home);
    f(&mut state);
    let mut tmp = tempfile::NamedTempFile::new_in(home)?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), &state)?;
    tmp.persist(state_path(home)).map_err(|e| e.error)?;
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
    crate::vault::tmp_files(home)
        .into_iter()
        .filter(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= STALE_TMP)
        })
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// Create a snapshot on request and record it as the latest automatic one,
/// so a previous failure stops being reported.
pub fn snapshot_now(home: &Path) -> Result<Value> {
    let r = crate::snapshots::create(home)?;
    update(home, |state| {
        state["snapshot_attempt_at"] = json!(now_secs());
        state["snapshot_retry_at"] = Value::Null;
        state["snapshot"] = json!({"ok": true, "at": crate::now_iso(), "id": r["id"]});
    })?;
    Ok(r)
}

/// Self-check findings worth surfacing: warn/fail. `skip` (the check stepped
/// aside because collection was running) is neutral, and "no snapshot yet" is
/// only a problem while automatic snapshots are on and one has succeeded.
fn doctor_problems(report: &Value, snapshot_counts: bool) -> Vec<Value> {
    report["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["status"] != "ok" && c["status"] != "skip")
        .filter(|c| c["name"] != "db_snapshot" || snapshot_counts)
        .cloned()
        .collect()
}

/// Record a manual self-check (settings page) as the latest result, on a par
/// with the daily background one: a clean run clears stale problems — such as
/// an orphan count that raced a collection — without waiting for tomorrow.
pub fn record_doctor(home: &Path, report: &Value) -> Result<()> {
    let snapshot_counts = snapshot_interval_days(home) > 0 && load(home)["snapshot"]["ok"] == true;
    let tmp_removed = load(home)["doctor"]["tmp_removed"].as_u64().unwrap_or(0);
    update(home, |state| {
        state["doctor_attempt_at"] = json!(now_secs());
        state["doctor"] = json!({
            "ok": report["ok"],
            "at": report["checked_at"],
            "problems": doctor_problems(report, snapshot_counts),
            "tmp_removed": tmp_removed,
        });
    })
}

/// Run whatever is due. Each task records its attempt time, so a failing task
/// is retried on its next interval rather than on every collection pass.
pub fn run_due(home: &Path) -> Result<Value> {
    let state = load(home);
    let now = now_secs();
    let mut ran = Vec::new();

    // Earlier versions wrote one file per object; move them into the object
    // store a batch per pass so collection is never held up for long.
    let mut migrating = false;
    if crate::vault::objects_root(home).is_dir() {
        match crate::vault::migrate_legacy(home, MIGRATE_BATCH) {
            Ok(r) if r["moved"].as_u64().unwrap_or(0) > 0 => {
                migrating = r["remaining"] == true;
                update(home, |state| {
                    let moved = state["migration"]["moved"].as_u64().unwrap_or(0) + r["moved"].as_u64().unwrap_or(0);
                    state["migration"] = json!({"moved": moved, "remaining": r["remaining"], "skipped": r["skipped"]});
                })?;
                ran.push("migration");
            }
            Ok(_) => {}
            Err(e) => {
                migrating = true;
                eprintln!("yourmem object migration: {e:#}");
            }
        }
    }

    let interval_days = snapshot_interval_days(home);
    // 迁移未完成时快照要逐个读旧文件，等迁完再做
    let retry_wait = state["snapshot_retry_at"].as_u64().is_some_and(|t| now < t);
    if interval_days > 0
        && !migrating
        && !retry_wait
        && due(&state["snapshot_attempt_at"], interval_days * 86_400, now)
    {
        let result = crate::snapshots::create_within(home, LOCK_WAIT);
        update(home, |state| match result {
            Ok(r) => {
                state["snapshot_attempt_at"] = json!(now);
                state["snapshot_retry_at"] = Value::Null;
                state["snapshot"] = json!({"ok": true, "at": crate::now_iso(), "id": r["id"]});
            }
            // 被采集或其他操作占着锁：不算失败，不推迟到下个周期，过一会儿再试
            Err(e) if is_busy(&e) => {
                state["snapshot_retry_at"] = json!(now + BUSY_RETRY_SECS);
                if state["snapshot"]["ok"] != true {
                    state["snapshot"] = json!({"ok": false, "retry": true, "at": crate::now_iso()});
                }
            }
            Err(e) => {
                state["snapshot_attempt_at"] = json!(now);
                state["snapshot_retry_at"] = Value::Null;
                state["snapshot"] = json!({"ok": false, "at": crate::now_iso(), "error": format!("{e:#}")});
            }
        })?;
        ran.push("snapshot");
    }

    if due(&state["doctor_attempt_at"], DOCTOR_INTERVAL_SECS, now) {
        let tmp_removed = remove_stale_tmp(home);
        let report = crate::db::open(home).and_then(|conn| crate::doctor::run(&conn, home));
        // 快照状态取最新（自检期间可能刚有一次手动快照）
        let snapshot_ok = load(home)["snapshot"]["ok"] == true;
        let doctor = match report {
            Ok(r) => {
                json!({
                    "ok": r["ok"],
                    "at": r["checked_at"],
                    "problems": doctor_problems(&r, interval_days > 0 && snapshot_ok),
                    "tmp_removed": tmp_removed,
                })
            }
            Err(e) => json!({
                "ok": false,
                "at": crate::now_iso(),
                "problems": [{"name": "doctor", "status": "fail", "detail": format!("{e:#}")}],
                "tmp_removed": tmp_removed,
            }),
        };
        update(home, |state| {
            state["doctor_attempt_at"] = json!(now);
            state["doctor"] = doctor;
        })?;
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
    fn zero_interval_turns_snapshots_off() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        crate::ingest::write_config(home, &json!({"snapshot_interval_days": 0})).unwrap();
        let r = run_due(home).unwrap();
        assert_eq!(r["ran"], json!(["doctor"]));
        let problems = status(home)["doctor"]["problems"].clone();
        assert!(problems.as_array().unwrap().iter().all(|p| p["name"] != "db_snapshot"), "{problems}");
    }

    #[test]
    fn snapshot_waits_for_legacy_migration() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        for i in 0..(MIGRATE_BATCH + 1) {
            let bytes = format!("legacy line {i}").into_bytes();
            let path = crate::vault::object_path(home, &crate::vault::hash_bytes(&bytes));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let first = run_due(home).unwrap();
        assert_eq!(first["ran"], json!(["migration", "doctor"]), "snapshot waits while files remain");
        let second = run_due(home).unwrap();
        assert_eq!(second["ran"], json!(["migration", "snapshot"]));
    }

    #[test]
    fn a_held_lock_defers_the_snapshot_instead_of_failing_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        let lock = crate::ingest::ImportLockTx::acquire(home, Duration::from_secs(1)).unwrap();
        let r = run_due(home).unwrap();
        assert!(r["ran"].as_array().unwrap().contains(&json!("snapshot")));
        let st = status(home);
        assert_eq!(st["snapshot"]["retry"], true, "{st}");
        assert!(st["snapshot_attempt_at"].is_null(), "未记为一次尝试，不等整个周期");
        assert!(st["snapshot_retry_at"].as_u64().unwrap() > now_secs());
        assert!(st["doctor"]["problems"].as_array().unwrap().iter().all(|p| p["name"] != "db_snapshot"),
            "首份快照未完成时不提示快照过旧：{st}");
        drop(lock);

        // 等待时间到后自动补做
        update(home, |state| state["snapshot_retry_at"] = json!(now_secs() - 1)).unwrap();
        run_due(home).unwrap();
        let st = status(home);
        assert_eq!(st["snapshot"]["ok"], true, "{st}");
        assert!(st["snapshot_retry_at"].is_null());
    }

    #[test]
    fn state_from_1_3_0_lock_failures_is_retried_and_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        update(home, |state| *state = json!({
            "snapshot_attempt_at": now_secs(),
            "snapshot": {"ok": false, "at": "2026-10-01T01:00:00Z", "error": "database is locked"},
            "doctor_attempt_at": now_secs(),
            "doctor": {"ok": true, "problems": [{"name": "db_snapshot", "status": "warn", "detail": "尚无快照"}]},
        })).unwrap();
        let st = status(home);
        assert_eq!(st["snapshot"]["retry"], true);
        assert!(st["doctor"]["problems"].as_array().unwrap().is_empty(), "{st}");
        let r = run_due(home).unwrap();
        assert!(r["ran"].as_array().unwrap().contains(&json!("snapshot")), "立即重试：{r}");
        assert_eq!(status(home)["snapshot"]["ok"], true);
    }

    #[test]
    fn a_clean_manual_doctor_clears_stale_problems() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        crate::db::open(home).unwrap();
        crate::ingest::write_config(home, &json!({"snapshot_interval_days": 0})).unwrap();
        // 后台自检留下的旧问题：撞上采集中间态的孤儿误报
        update(home, |state| {
            state["doctor_attempt_at"] = json!(now_secs());
            state["doctor"] = json!({"ok": true, "at": "2026-10-08T09:27:31Z", "tmp_removed": 3,
                "problems": [{"name": "orphan_objects", "status": "warn", "detail": "9 个对象无引用"}]});
        }).unwrap();

        // 手动自检撞上采集：孤儿检查让路（skip），整体仍干净
        let report = json!({"ok": true, "checked_at": "2026-10-08T10:00:00Z", "checks": [
            {"name": "orphan_objects", "status": "skip", "detail": "正在采集，本轮跳过"},
            {"name": "fts", "status": "ok", "detail": "一致"}]});
        record_doctor(home, &report).unwrap();

        let st = status(home);
        assert!(st["doctor"]["problems"].as_array().unwrap().is_empty(), "skip 与干净自检都清掉旧问题：{st}");
        assert_eq!(st["doctor"]["tmp_removed"], 3, "手动自检不清 tmp，保留原计数");
        // 与后台自检同权记为一次尝试：当期不再重复跑
        assert_eq!(run_due(home).unwrap()["ran"], json!([]));
    }
}
