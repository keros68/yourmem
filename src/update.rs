//! 检查更新：当前版本 vs GitHub 最新 release。
//! 仓库已公开——走匿名公开 API，用系统自带的 curl 拉取（Windows 10 1803+ /
//! macOS / Linux 均内置），零新增依赖、不需要任何登录。匿名限频 60 次/时/IP，
//! 启动一次 + 手动检查远用不完。早先的 gh CLI 方案是私有仓库时代的产物，
//! 普通用户机器上没有 gh，已退役。

use anyhow::{bail, Result};
use serde_json::{json, Value};

const RELEASE_REPO: &str = "keros68/yourmem";

/// 最新 release 的 tag / 发布页 / 发布时间（公开 API 瘦身后的结果）。
pub fn latest_release() -> Result<Value> {
    let url = format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest");
    let out = std::process::Command::new("curl")
        .args([
            "-sfSL",
            "--max-time",
            "10",
            "-A",
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("无法运行 curl：{e}"))?;
    if !out.status.success() {
        bail!("Release 查询失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(json!({
        "tag": v["tag_name"].as_str().unwrap_or(""),
        "url": v["html_url"].as_str().unwrap_or(""),
        "published_at": v["published_at"].as_str().unwrap_or(""),
    }))
}

/// 语义化版本比较：容忍 v/V 前缀、逐段数值、缺段补 0；
/// latest 严格大于 current 才算有更新（相同/更旧/非版本号一律 false）。
pub fn version_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .split(['.', '-'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_newer_semantics() {
        assert!(version_newer("v1.0.2", "1.0.1"), "v 前缀容忍");
        assert!(version_newer("1.1", "1.0.9"), "缺段补 0");
        assert!(version_newer("v2.0.0", "1.9.9"));
        assert!(!version_newer("v1.0.1", "1.0.1"), "相同不算");
        assert!(!version_newer("v1.0.0", "1.0.1"), "更旧不算");
        assert!(!version_newer("garbage", "1.0.1"), "非版本号不算");
    }
}
