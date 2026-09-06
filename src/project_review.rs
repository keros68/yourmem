//! 显式记录项目 Git 基线；变化仅提示复查，不推断记忆失效。
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn git(path: &str, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args([
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-C",
        path,
    ])
    .args(args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Git 检测超时");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn baseline_path(home: &Path, path: &str) -> PathBuf {
    home.join("review-baselines")
        .join(format!("{:x}.json", Sha256::digest(path.as_bytes())))
}

pub fn status(conn: &Connection, home: &Path, pid: i64) -> Result<Value> {
    let path: String =
        conn.query_row("SELECT path FROM projects WHERE id=?1", [pid], |r| r.get(0))?;
    let baseline = std::fs::read(baseline_path(home, &path))
        .ok()
        .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
        .unwrap_or(Value::Null);
    let mut value = json!({"path": path, "baseline": baseline, "status": "unavailable"});
    let detected = (|| -> Result<(String, bool)> {
        let head = git(&path, &["rev-parse", "--verify", "HEAD"])?;
        anyhow::ensure!(head.status.success(), "项目没有可读取的 Git 提交");
        let head = String::from_utf8(head.stdout)?.trim().to_string();
        let dirty = git(
            &path,
            &[
                "diff",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ],
        )?;
        anyhow::ensure!(
            matches!(dirty.status.code(), Some(0 | 1)),
            "无法检查已跟踪文件"
        );
        Ok((head, dirty.status.code() == Some(1)))
    })();
    match detected {
        Ok((head, dirty)) => {
            value["status"] = json!(if baseline["head"].as_str().is_none() {
                "unreviewed"
            } else if baseline["head"] != head || dirty {
                "changed"
            } else {
                "unchanged"
            });
            value["head"] = json!(head);
            value["tracked_changes"] = json!(dirty);
        }
        Err(_) => {
            value["reason"] = json!("Git 不可用、没有提交或检测超时");
        }
    }
    Ok(value)
}

pub fn mark_reviewed(conn: &Connection, home: &Path, pid: i64) -> Result<Value> {
    let current = status(conn, home, pid)?;
    let head = current["head"]
        .as_str()
        .context("无法读取 Git 提交，未记录基线")?;
    anyhow::ensure!(
        current["tracked_changes"] == false,
        "已跟踪文件尚有修改，无法记录提交基线"
    );
    let path = current["path"].as_str().context("项目路径缺失")?;
    let target = baseline_path(home, path);
    std::fs::create_dir_all(target.parent().unwrap())?;
    let tmp = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(
        &tmp,
        serde_json::to_vec(&json!({"head": head, "reviewed_at": crate::now_iso()}))?,
    )?;
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(tmp);
        return Err(e.into());
    }
    status(conn, home, pid)
}
