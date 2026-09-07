//! 隐私守门（2026-09-06 公开库最小面）：git 跟踪的文件不得携带真实身份、
//! 本机路径或内部课题词，命中即失败——防内部笔记再次混入公开提交。
//! 白名单只放测试占位符与仓库 slug；git 不可用（发布 tarball 等环境）时跳过。

use std::process::Command;

/// Optional local-only terms; never commit real identities to the scanner.
fn private_terms() -> Vec<String> {
    let path = std::env::var_os("YOURMEM_PRIVACY_DENY_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".privacy-deny.json"));
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).expect("privacy terms must be a JSON string array"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("cannot read privacy terms: {e}"),
    }
}

/// 通用个人主目录前缀（占位符用户名在 ALLOW 内放行）
const PATH_PREFIXES: [&str; 4] = ["/Users/", "C:\\Users\\", "C:/Users/", "/c/Users/"];
const ALLOW_USERS: &[&str] = &["test", "devuser", "a", "x", "user"];

fn personal_email_hit(line: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || "._%+-@".contains(c)))
        .any(|token| token.split_once('@').is_some_and(|(name, domain)| {
            !name.is_empty() && domain.contains('.') &&
                !["example.com", "example.org", "example.net", "example.invalid", "2x.png"].contains(&domain.trim_end_matches('.'))
        }))
}

fn personal_path_hit(line: &str) -> Option<String> {
    let normalized = line.replace("\\\\", "\\");
    let line = normalized.as_str();
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
    let output = match Command::new("git").args(["-c", "core.quotePath=false", "ls-files", "-z", "--cached", "--others", "--exclude-standard"]).output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("git 不可用，跳过隐私扫描");
            return;
        }
    };
    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    assert!(!files.is_empty(), "git ls-files 无输出，扫描对象为空不可信");

    let terms = private_terms();
    let mut hits: Vec<String> = Vec::new();
    for file in &files {
        let Ok(content) = std::fs::read_to_string(file) else { continue };
        for (idx, line) in content.lines().enumerate() {
            for pat in &terms {
                if line.contains(pat) {
                    hits.push(format!("{file}:{} 含 \"{pat}\"", idx + 1));
                }
            }
            if personal_email_hit(line) {
                hits.push(format!("{file}:{} contains a personal email", idx + 1));
            }
            if let Some(desc) = personal_path_hit(line) {
                hits.push(format!("{file}:{} {desc}", idx + 1));
            }
        }
    }
    assert!(hits.is_empty(), "公开跟踪文件含隐私内容:\n{}", hits.join("\n"));
}

#[test]
fn scanner_detects_synthetic_paths_and_emails() {
    let path = [r"C:\Users\", r"synthetic-person\notes.txt"].concat();
    assert!(personal_path_hit(&path).is_some());
    assert!(personal_path_hit(&path.replace('\\', "\\\\")).is_some());
    assert!(personal_path_hit("/Users/devuser/project").is_none());
    let email = ["synthetic", "synthetic-provider.org"].join("@");
    assert!(personal_email_hit(&email));
    assert!(!personal_email_hit("user@example.com"));
}
