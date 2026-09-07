//! Native agent memory file backup (DESIGN-0.3 §2).
//!
//! Agents keep their own memory/instruction files (Claude's auto memory
//! `MEMORY.md`, `~/.codex/AGENTS.md`, project `CLAUDE.md`…). Unlike append-only
//! session JSONL these are small files rewritten in place, so we snapshot the
//! whole file into the vault and keep a revision history. Backup only — the
//! semantic layer never parses them, and they are presented separately from
//! the curated `memories` table.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::{db, vault};

/// Agent config dirs that hold native memory files. Defaults derive from
/// $HOME; tests inject tempdirs.
pub struct SourceDirs {
    pub claude: PathBuf, // ~/.claude
    pub codex: PathBuf,  // ~/.codex
}

impl Default for SourceDirs {
    fn default() -> Self {
        Self {
            claude: crate::home_dir().join(".claude"),
            codex: crate::home_dir().join(".codex"),
        }
    }
}

#[derive(Default)]
pub struct MemfilesOutcome {
    pub files_monitored: usize,
    pub files_changed: usize,
    pub revisions_added: usize,
}

/// Claude encodes a project cwd into `~/.claude/projects/<enc>`; the exact
/// encoding has drifted between versions, so generate the plausible variants
/// and let the on-disk existence check decide.
fn claude_project_encodings(cwd: &str) -> Vec<String> {
    let mut out = vec![cwd.replace('/', "-")];
    for candidate in [
        cwd.replace('\\', "-"),             // Windows 反斜杠分隔的 cwd
        cwd.replace(['/', '\\', ':'], "-"), // Windows 全量归一（盘符冒号不入目录名）
        out[0].replace('.', "-"),
        out[0].replace('.', "-").replace('_', "-"),
    ] {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Discover monitor targets: global instruction files plus per-project files
/// discovered from imported sessions' cwd (never a proactive disk scan).
/// OpenCode: 本机实测其配置目录只有 json 配置、无 markdown 形态的 memory/
/// instruction 文件，无可备份对象，暂不监控（DESIGN-0.3 §2.2 "实施时确认"的结论）。
fn discover_targets(conn: &Connection, dirs: &SourceDirs) -> Result<Vec<(String, String, PathBuf)>> {
    let mut targets: Vec<(String, String, PathBuf)> = Vec::new(); // (agent, scope, path)

    for (agent, path) in [
        (crate::adapters::AGENT_CLAUDE, dirs.claude.join("CLAUDE.md")),
        (crate::adapters::AGENT_CODEX, dirs.codex.join("AGENTS.md")),
    ] {
        if path.is_file() {
            targets.push((agent.to_string(), "global".to_string(), path));
        }
    }

    // Per-project: session cwds -> project CLAUDE.md + Claude auto memory dir.
    // 有意不过滤 deleted_at（自检 C4 记档）：监控目标的发现是"这个项目存在过
    // CLAUDE.md"的历史事实，回收站里的会话同样见证过项目根——过滤会让项目级
    // memory 备份随回收站消失。
    let mut stmt = conn.prepare("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL")?;
    let cwds: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for cwd in cwds {
        let (root, _) = db::project_root_for(&cwd);
        let scope = format!("project:{root}");

        let project_claude_md = Path::new(&cwd).join("CLAUDE.md");
        if project_claude_md.is_file() {
            targets.push((crate::adapters::AGENT_CLAUDE.to_string(), scope.clone(), project_claude_md));
        }

        // Codex project instructions use AGENTS.md, independently of CLAUDE.md.
        let project_agents = Path::new(&cwd).join("AGENTS.md");
        if project_agents.is_file() {
            targets.push((crate::adapters::AGENT_CODEX.to_string(), scope.clone(), project_agents));
        }

        for enc in claude_project_encodings(&cwd) {
            let mem_dir = dirs.claude.join("projects").join(enc).join("memory");
            if mem_dir.is_dir() {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&mem_dir)?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect();
                files.sort();
                for f in files {
                    targets.push((crate::adapters::AGENT_CLAUDE.to_string(), scope.clone(), f));
                }
                break; // first existing encoding wins
            }
        }
    }

    targets.sort_by(|a, b| a.2.cmp(&b.2));
    targets.dedup_by(|a, b| a.2 == b.2);
    Ok(targets)
}

/// Snapshot every monitored file whose content changed since the last import.
/// Files are all KB-scale, so we read + hash unconditionally instead of
/// maintaining mtime/size state — simpler and immune to mtime games.
pub fn collect(conn: &Connection, home: &Path, dirs: &SourceDirs) -> Result<MemfilesOutcome> {
    let mut out = MemfilesOutcome::default();
    for (agent, scope, path) in discover_targets(conn, dirs)? {
        out.files_monitored += 1;
        // 单文件读失败只跳过该文件不连坐整轮采集（与 UTF-8 不完整"留到下一轮"
        // 同口径，自检 C1）——权限/占位锁这类瞬时问题下轮自愈
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("yourmem memfiles: 跳过读不了的 {}（下轮重试）", path.display());
            continue;
        };
        // UTF-8 完整性校验：agent 正在写的文件（多字节字符被切断）留到下一轮。
        let Ok(text) = std::str::from_utf8(&bytes) else { continue };
        let hash = vault::hash_bytes(&bytes);
        let path_str = path.to_string_lossy().to_string();
        // 先落 vault 再动库：内容寻址，重复写同一对象无害；若 upsert 先行而
        // store_bytes 中途失败，current_hash 会指向缺失对象，下一轮 changed=false
        // 永久跳过该文件。库侧三步包一个事务，失败整体回滚下轮重来。
        vault::store_bytes(home, &bytes)?;
        let tx = conn.unchecked_transaction()?;
        let (file_id, changed) = db::upsert_memory_file(&tx, &agent, &scope, &path_str, &hash)?;
        if !changed {
            continue;
        }
        db::insert_memory_revision(&tx, file_id, &hash, bytes.len() as u64)?;
        db::set_memory_fts(&tx, file_id, text)?;
        tx.commit()?;
        out.files_changed += 1;
        out.revisions_added += 1;
    }
    Ok(out)
}

/// Full view of one file: metadata + revision timeline + content of the
/// chosen revision (default: latest). Content is hash-verified on read.
pub fn show(conn: &Connection, home: &Path, file_id: i64, revision: Option<i64>) -> Result<Value> {
    let file = db::memory_file_by_id(conn, file_id)?
        .ok_or_else(|| anyhow::anyhow!("memory file not found: {file_id}"))?;
    let revisions = db::memory_file_revisions(conn, file_id)?;
    let chosen = match revision {
        Some(rid) => revisions
            .iter()
            .find(|r| r["id"].as_i64() == Some(rid))
            .ok_or_else(|| anyhow::anyhow!("revision not found: {rid}"))?,
        None => revisions.first().ok_or_else(|| anyhow::anyhow!("no revisions yet"))?,
    };
    let bytes = vault::read_object(home, chosen["hash"].as_str().unwrap_or_default(), true)?;
    Ok(json!({
        "file": file,
        "revision": chosen,
        "revisions": revisions,
        "content": String::from_utf8_lossy(&bytes),
    }))
}

/// Diff the last `last` revisions (oldest -> newest, consecutive pairs).
pub fn diff(conn: &Connection, home: &Path, file_id: i64, last: usize) -> Result<Value> {
    let file = db::memory_file_by_id(conn, file_id)?
        .ok_or_else(|| anyhow::anyhow!("memory file not found: {file_id}"))?;
    let mut revisions = db::memory_file_revisions(conn, file_id)?;
    revisions.reverse(); // oldest first
    let window = revisions.split_off(revisions.len().saturating_sub(last.max(2)));

    let mut pairs = Vec::new();
    for w in window.windows(2) {
        let read = |r: &Value| -> Result<String> {
            let bytes = vault::read_object(home, r["hash"].as_str().unwrap_or_default(), true)?;
            Ok(String::from_utf8_lossy(&bytes).to_string())
        };
        let (old, new) = (read(&w[0])?, read(&w[1])?);
        pairs.push(json!({
            "from": w[0],
            "to": w[1],
            "diff": line_diff(&old, &new),
        }));
    }
    Ok(json!({ "file": file, "diffs": pairs }))
}

/// FTS search over latest revisions, with a <3-char fallback that substring-
/// matches vault contents directly (trigram cannot index shorter tokens;
/// same convention as `db::search`).
pub fn search(conn: &Connection, home: &Path, query: &str, limit: u32) -> Result<Vec<Value>> {
    let use_fts = query.split_whitespace().all(|t| t.chars().count() >= 3) && !query.trim().is_empty();
    if use_fts {
        return db::search_memory_files_fts(conn, query, limit);
    }
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    for f in db::list_memory_files(conn)? {
        // 搜索是索引性扫描：不校验哈希（show/diff 才做保真校验），单文件对象
        // 缺失跳过不炸整次搜索（自检 C1）
        let Ok(bytes) = vault::read_object(home, f["current_hash"].as_str().unwrap_or_default(), false) else {
            continue;
        };
        if String::from_utf8_lossy(&bytes).to_lowercase().contains(&needle) {
            hits.push(json!({
                "id": f["id"], "agent": f["agent"], "scope": f["scope"],
                "path": f["path"], "updated_at": f["updated_at"],
            }));
        }
        if hits.len() >= limit as usize {
            break;
        }
    }
    Ok(hits)
}

/// MCP `read_native_memory`: latest verified content + revision metadata for
/// one agent's monitored files (optionally narrowed to a single path suffix).
pub fn read_native(conn: &Connection, home: &Path, agent: &str, path: Option<&str>) -> Result<Value> {
    let files: Vec<Value> = db::list_memory_files(conn)?
        .into_iter()
        .filter(|f| f["agent"].as_str() == Some(agent))
        .filter(|f| match path {
            // 后缀必须落在路径分隔符上：裸 ends_with 会拿 "y.md" 命中 MEMORY.md（自检 C1）。
            // 分隔符归一化：Windows 路径用 \，统一换成 / 再比较。
            Some(p) => f["path"]
                .as_str()
                .map(|fp| {
                    let fp = fp.replace('\\', "/");
                    let p = p.replace('\\', "/");
                    fp == p || fp.ends_with(&format!("/{p}"))
                })
                .unwrap_or(false),
            None => true,
        })
        .collect();
    let mut out = Vec::new();
    for f in files {
        let bytes = vault::read_object(home, f["current_hash"].as_str().unwrap_or_default(), true)?;
        out.push(json!({
            "id": f["id"],
            "scope": f["scope"],
            "path": f["path"],
            "revisions": f["revisions"],
            "updated_at": f["updated_at"],
            "content": String::from_utf8_lossy(&bytes),
        }));
    }
    Ok(json!({ "memory_files": out }))
}

/// Minimal dependency-free line diff (LCS on lines): ' ' context, '-' removed,
/// '+' added. Memory files are KB-scale, so an O(n·m) DP table is fine.
fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = LCS length of a[i..], b[j..]
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push_str(&format!("  {}\n", a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str(&format!("- {}\n", a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+ {}\n", b[j]));
            j += 1;
        }
    }
    for line in &a[i..] {
        out.push_str(&format!("- {line}\n"));
    }
    for line in &b[j..] {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}
