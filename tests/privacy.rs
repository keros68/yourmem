//! 隐私守门（2026-09-06 公开库最小面）：git 跟踪的文件不得携带真实身份、
//! 本机路径或内部课题词，命中即失败——防内部笔记再次混入公开提交。
//! 白名单只放测试占位符与仓库 slug；git 不可用（发布 tarball 等环境）时跳过。

use std::process::Command;

/// 精确子串黑名单：真名/邮箱/本机路径/内部课题词（含 Rust 源码转义形态）
const DENY_SUBSTRINGS: &[&str] = &[
    "guoxiaoyu",
    "Xiaoyu Guo",
    "keros68@gmail.com",
    "C:\\Users\\keros68",
    "C:\\\\Users\\\\keros68",
    "C:/Users/keros68",
    "/c/Users/keros68",
    "/Users/guoxiaoyu",
    "work\\yourmem",
    "work\\\\yourmem",
    "work/yourmem",
    "大同",
    "盐渍",
    "salinity",
];

/// 通用个人主目录前缀（占位符用户名在 ALLOW 内放行）
const PATH_PREFIXES: [&str; 4] = ["/Users/", "C:\\Users\\", "C:/Users/", "/c/Users/"];
const ALLOW_USERS: &[&str] = &["test", "devuser", "a", "x", "user"];

/// keros68 是公开 GitHub 用户名，唯一合法形态是仓库 slug（update 检查用）
fn keros68_violation(line: &str) -> bool {
    line.contains("keros68") && !line.contains("keros68/yourmem")
}

fn personal_path_hit(line: &str) -> Option<String> {
    for prefix in PATH_PREFIXES {
        let mut from = 0;
        while let Some(pos) = line[from..].find(prefix) {
            let abs = from + pos + prefix.len();
            from = abs;
            let name: String = line[abs..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
                .collect();
            if !name.is_empty()
                && !name.chars().all(|c| c == '.') // "/Users/..." 文档占位符
                && !ALLOW_USERS.contains(&name.as_str())
            {
                return Some(format!("个人路径 …{prefix}{name}（白名单外用户名）"));
            }
        }
    }
    None
}

#[test]
fn tracked_files_carry_no_personal_data() {
    let output = match Command::new("git").args(["ls-files"]).output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("git 不可用，跳过隐私扫描");
            return;
        }
    };
    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    assert!(!files.is_empty(), "git ls-files 无输出，扫描对象为空不可信");

    let mut hits: Vec<String> = Vec::new();
    for file in &files {
        let Ok(content) = std::fs::read_to_string(file) else { continue };
        for (idx, line) in content.lines().enumerate() {
            for pat in DENY_SUBSTRINGS {
                if line.contains(pat) {
                    hits.push(format!("{file}:{} 含 \"{pat}\"", idx + 1));
                }
            }
            if keros68_violation(line) {
                hits.push(format!("{file}:{} 含未白名单化的 keros68", idx + 1));
            }
            if let Some(desc) = personal_path_hit(line) {
                hits.push(format!("{file}:{} {desc}", idx + 1));
            }
        }
    }
    assert!(hits.is_empty(), "公开跟踪文件含隐私内容:\n{}", hits.join("\n"));
}
