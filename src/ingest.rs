//! Session ingest: incremental import + vault archiving in one pass.
//!
//! Crash safety: message inserts, vault manifest rows and the source-file
//! offset advance in a single transaction, so a kill mid-file never leaves
//! half-imported state. A trailing partial line (agent still writing) is
//! left for the next run — neither parsed nor archived yet.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::adapters;
use crate::db;
use crate::vault;

pub struct ImportOutcome {
    pub files_seen: usize,
    pub files_updated: usize,
    /// Windows 文件占用锁：agent 正在写时读取被拒（PermissionDenied），本轮
    /// 跳过该文件——增量采集设计下 offset 未推进，下轮自然补采，无损。
    pub files_skipped: usize,
    pub messages_added: u64,
    pub lines_archived: u64,
    pub opencode_sessions_updated: usize,
    pub lineage_links: u64,
    /// Native memory file backup (DESIGN-0.3 §2), filled by import_with
    /// (`memfiles::collect` runs after import_all, not inside it, so tests of
    /// import_all never touch the real ~/.claude).
    pub memory_files_monitored: usize,
    pub memory_revisions_added: usize,
}

/// import 后半段共用：增量导入 + 原生 memory 采集 + `.last_import` 单飞时钟戳（MCP init 用，watch/CLI/app 同钟）。
pub fn import_with(
    home: &Path,
    roots: &[(&str, PathBuf)],
    opencode_db: Option<&Path>,
    hermes_db: Option<&Path>,
) -> Result<ImportOutcome> {
    let result = import_with_inner(home, roots, opencode_db, hermes_db);
    crate::recall_status::record(home, "ingest", "import", result.is_ok(), result.as_ref().ok().map(|o| o.messages_added));
    result
}

fn import_with_inner(home: &Path, roots: &[(&str, PathBuf)], opencode_db: Option<&Path>, hermes_db: Option<&Path>) -> Result<ImportOutcome> {
    // 锁覆盖全程：import_all + memfiles 采集 + .last_import（codex 复审：只锁
    // import_all 时，并发 import_with 可在 memfiles 的"读旧 hash→更新"竞态里
    // 重复插修订/撞 UNIQUE(agent,path)）。fail-closed：拿不到锁就报错——
    // 宁可这轮失败也不打开并发重复的窗口。
    let _lock = ImportLockTx::acquire(home, std::time::Duration::from_secs(30))?;
    let mut conn = db::open(home)?;
    let mut outcome = import_all(&mut conn, home, roots, opencode_db, hermes_db)?;
    let mf = crate::memfiles::collect(&conn, home, &crate::memfiles::SourceDirs::default())?;
    outcome.memory_files_monitored = mf.files_monitored;
    outcome.memory_revisions_added = mf.revisions_added;
    let _ = std::fs::File::create(home.join(".last_import"));
    Ok(outcome)
}

/// 默认数据源的整库导入（CLI/app/MCP init 共用）；根目录可用
/// YOUMEM_CLAUDE_DIR / YOUMEM_CODEX_DIR / YOUMEM_OPENCODE_DB 覆盖（测试与调试）。
/// 另合并 config.json 里登记的自定义根（app 设置页 / `agents add`）。
/// 去重键与登记键一致（agent, path）：add_extra_root 允许不同 agent 登记同一路径，
/// 只按 path 去重会静默丢弃后一个 agent 的采集根（codex 评审 blocker）
fn push_extra_root(roots: &mut Vec<(String, PathBuf)>, agent: String, path: PathBuf) {
    if !roots.iter().any(|(a, p)| *a == agent && *p == path) {
        roots.push((agent, path));
    }
}

pub fn import_defaults(home: &Path) -> Result<ImportOutcome> {
    let pick = |var: &str, d: PathBuf| std::env::var_os(var).map(PathBuf::from).unwrap_or(d);
    let off = disabled_agents(home);
    let off_root = |agent: &str, var: &str, d: PathBuf| -> Option<PathBuf> {
        // 停用的 agent 不再作为采集源（YOUMEM_*_DIR 覆盖同样被拦——停用是
        // 明确的"不要这个源"，与目录在哪无关）。已卸载的源默认根本来就不存在，
        // 双重保险。
        if off.iter().any(|a| a == agent) {
            return None;
        }
        Some(pick(var, d))
    };
    let mut roots: Vec<(String, PathBuf)> = [
        (adapters::AGENT_CLAUDE, "YOUMEM_CLAUDE_DIR", crate::default_claude_root()),
        (adapters::AGENT_CODEX, "YOUMEM_CODEX_DIR", crate::default_codex_root()),
        (adapters::AGENT_ZCODE, "YOUMEM_ZCODE_DIR", crate::default_zcode_root()),
        (adapters::AGENT_KIMI, "YOUMEM_KIMI_DIR", crate::default_kimi_root()),
        (adapters::AGENT_PI, "YOUMEM_PI_DIR", crate::default_pi_root()),
    ]
    .into_iter()
    .filter_map(|(a, v, d)| off_root(a, v, d).map(|p| (a.to_string(), p)))
    .collect();
    for (agent, path) in extra_roots(home) {
        push_extra_root(&mut roots, agent, path);
    }
    let refs: Vec<(&str, PathBuf)> = roots.iter().map(|(a, p)| (a.as_str(), p.clone())).collect();
    let oc = pick("YOUMEM_OPENCODE_DB", adapters::opencode::default_db_path());
    let oc = if off.iter().any(|a| a == adapters::opencode::AGENT_OPENCODE) { None } else { Some(oc) };
    let hm = pick("YOUMEM_HERMES_DB", adapters::hermes::default_db_path());
    let hm = if off.iter().any(|a| a == adapters::hermes::AGENT_HERMES) { None } else { Some(hm) };
    import_with(home, &refs, oc.as_deref(), hm.as_deref())
}

// ---------------------------------------------------------------- 自定义数据源根目录
// 用户在 app 设置页（或 `agents add`）登记的额外采集根，持久化在
// $YOUMEM_HOME/config.json 的 extra_roots。只支持文件型 agent
// （claude/codex/zcode/kimi）；opencode 是单库源，用 YOUMEM_OPENCODE_DB 覆盖。

/// 可登记额外根的文件型 agent。
pub const EXTRA_ROOT_AGENTS: [&str; 5] = [
    adapters::AGENT_CLAUDE, adapters::AGENT_CODEX, adapters::AGENT_ZCODE, adapters::AGENT_KIMI,
    adapters::AGENT_PI,
];

pub fn read_config(home: &Path) -> Value {
    std::fs::read_to_string(home.join("config.json")).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub fn write_config(home: &Path, cfg: &Value) -> Result<()> {
    std::fs::create_dir_all(home)?;
    // 先 tmp 再 rename（vault 同纪律）：写入中途崩溃不留半个 JSON
    let tmp = home.join(format!("config.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg)?)?;
    std::fs::rename(&tmp, home.join("config.json"))?;
    Ok(())
}

/// config.json 里登记的自定义根（agent, path）。文件缺失/损坏按空处理。
pub fn extra_roots(home: &Path) -> Vec<(String, PathBuf)> {
    read_config(home)["extra_roots"].as_array().cloned().unwrap_or_default()
        .into_iter()
        .filter_map(|r| Some((r["agent"].as_str()?.to_string(), PathBuf::from(r["path"].as_str()?))))
        .collect()
}

/// CLI import 的根合并补全：config.json 的 extra_roots 追加进根表（去重），
/// 停用的 agent 整体剔除——与 import_defaults 同一套语义。CLI 的 run_import
/// 此前两条都漏（真机 2026-09-03：agents add 登记的根被 CLI import 忽略）。
pub fn apply_extra_roots_and_gates(home: &Path, roots: &mut Vec<(String, PathBuf)>) {
    let off = disabled_agents(home);
    roots.retain(|(a, _)| !off.iter().any(|d| d == a));
    for (agent, path) in extra_roots(home) {
        if !off.iter().any(|d| d == &agent) {
            push_extra_root(roots, agent, path);
        }
    }
}

/// 已停用的内置 agent（config.json disabled_agents，用户反馈 2026-08-29：卸载了
/// opencode 就不该再扫它的库）。停用 = 不采集不检测；已入库历史保留可搜。
pub fn disabled_agents(home: &Path) -> Vec<String> {
    read_config(home)["disabled_agents"].as_array().cloned().unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

pub fn set_agent_disabled(home: &Path, agent: &str, disabled: bool) -> Result<Value> {
    let known = [
        adapters::AGENT_CLAUDE, adapters::AGENT_CODEX, adapters::AGENT_ZCODE,
        adapters::AGENT_KIMI, adapters::AGENT_PI,
        adapters::opencode::AGENT_OPENCODE, adapters::hermes::AGENT_HERMES,
    ];
    anyhow::ensure!(known.contains(&agent), "unknown agent: {agent}");
    let mut cfg = read_config(home);
    let mut list: Vec<String> = disabled_agents(home);
    if disabled {
        if !list.iter().any(|a| a == agent) {
            list.push(agent.to_string());
        }
    } else {
        list.retain(|a| a != agent);
    }
    list.sort();
    cfg["disabled_agents"] = json!(list);
    write_config(home, &cfg)?;
    Ok(json!({ "ok": true, "agent": agent, "disabled": disabled }))
}

/// 主流 agent 观察名单（skills-manager 模式：自动检测目录，展示全貌）。
/// 只有目录级检测——解析需要专属 adapter（按"拿到真实样本再写"的门禁，
/// 见 AGENTS.md 明确不做清单），检测到只报"已发现目录"，不承诺采集。
pub const WATCHLIST: [(&str, fn() -> PathBuf); 7] = [
    ("gemini", || crate::home_dir().join(".gemini")),
    ("cursor", || crate::home_dir().join(".cursor")),
    ("copilot", || crate::home_dir().join(".copilot")),
    ("qwen", || crate::home_dir().join(".qwen")),
    ("crush", || crate::home_dir().join(".config").join("crush")),
    ("droid", || crate::home_dir().join(".factory")),
    ("trae", || crate::home_dir().join(".trae")),
];

pub fn add_extra_root(home: &Path, agent: &str, path: &Path) -> Result<Value> {
    anyhow::ensure!(EXTRA_ROOT_AGENTS.contains(&agent),
        "unsupported agent for extra root: {agent}（opencode 是单库源，用 YOUMEM_OPENCODE_DB 覆盖）");
    anyhow::ensure!(path.is_dir(), "not a directory: {}", path.display());
    let mut cfg = read_config(home);
    let mut roots = cfg["extra_roots"].as_array().cloned().unwrap_or_default();
    let p = path.to_string_lossy().to_string();
    anyhow::ensure!(!roots.iter().any(|r| r["agent"] == agent && r["path"] == p),
        "extra root already registered: {agent} {p}");
    roots.push(json!({ "agent": agent, "path": p }));
    cfg["extra_roots"] = Value::Array(roots);
    write_config(home, &cfg)?;
    Ok(json!({ "ok": true, "extra_roots": cfg["extra_roots"] }))
}

pub fn remove_extra_root(home: &Path, agent: &str, path: &Path) -> Result<Value> {
    let mut cfg = read_config(home);
    let roots = cfg["extra_roots"].as_array().cloned().unwrap_or_default();
    let p = path.to_string_lossy().to_string();
    let kept: Vec<Value> = roots.into_iter()
        .filter(|r| !(r["agent"] == agent && r["path"] == p)).collect();
    anyhow::ensure!(kept.len() < cfg["extra_roots"].as_array().map_or(0, Vec::len),
        "extra root not found: {agent} {p}");
    cfg["extra_roots"] = Value::Array(kept);
    write_config(home, &cfg)?;
    Ok(json!({ "ok": true, "extra_roots": cfg["extra_roots"] }))
}

/// 七源检测（app 设置页 / `agents` CLI）：默认根是否存在、库里各 agent 的
/// 会话数、已登记的自定义根、停用状态、最后采集时间；另附主流 agent 观察名单
/// 的目录级检测。只读。
pub fn agent_sources(home: &Path, conn: &Connection) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT agent, COUNT(*) FROM sessions WHERE deleted_at IS NULL GROUP BY agent",
    )?;
    let counts: std::collections::HashMap<String, i64> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    // 最后采集时间（source_files.updated_at）：文件型与单库源都记在 agent 列
    let mut stmt = conn.prepare("SELECT agent, MAX(updated_at) FROM source_files GROUP BY agent")?;
    let last_imported: std::collections::HashMap<String, String> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let extras = extra_roots(home);
    let disabled = disabled_agents(home);
    let is_disabled = |a: &str| disabled.iter().any(|d| d == a);
    let mut out = Vec::new();
    for (agent, root) in [
        (adapters::AGENT_CLAUDE, crate::default_claude_root()),
        (adapters::AGENT_CODEX, crate::default_codex_root()),
        (adapters::AGENT_ZCODE, crate::default_zcode_root()),
        (adapters::AGENT_KIMI, crate::default_kimi_root()),
        (adapters::AGENT_PI, crate::default_pi_root()),
    ] {
        out.push(json!({
            "agent": agent,
            "root": root,
            "detected": root.is_dir(),
            "disabled": is_disabled(agent),
            "sessions": counts.get(agent).copied().unwrap_or(0),
            "last_imported": last_imported.get(agent).map(String::as_str),
            "extra_roots": extras.iter().filter(|(a, _)| a == agent)
                .map(|(_, p)| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "can_add_root": true,
        }));
    }
    let oc = adapters::opencode::default_db_path();
    out.push(json!({
        "agent": adapters::opencode::AGENT_OPENCODE,
        "root": oc,
        "detected": oc.is_file(),
        "disabled": is_disabled(adapters::opencode::AGENT_OPENCODE),
        "sessions": counts.get(adapters::opencode::AGENT_OPENCODE).copied().unwrap_or(0),
        "last_imported": last_imported.get(adapters::opencode::AGENT_OPENCODE).map(String::as_str),
        "extra_roots": [],
        "can_add_root": false,  // 单库源：YOUMEM_OPENCODE_DB 覆盖
    }));
    let hm = adapters::hermes::default_db_path();
    out.push(json!({
        "agent": adapters::hermes::AGENT_HERMES,
        "root": hm,
        "detected": hm.is_file(),
        "disabled": is_disabled(adapters::hermes::AGENT_HERMES),
        "sessions": counts.get(adapters::hermes::AGENT_HERMES).copied().unwrap_or(0),
        "last_imported": last_imported.get(adapters::hermes::AGENT_HERMES).map(String::as_str),
        "extra_roots": [],
        "can_add_root": false,  // 单库源：YOUMEM_HERMES_DB 覆盖
    }));
    // 观察名单：目录级检测，解析支持待样本（不承诺采集）
    let watchlist: Vec<Value> = WATCHLIST.iter()
        .map(|(name, root)| json!({
            "agent": name,
            "root": root(),
            "detected": root().is_dir(),
        }))
        .collect();
    Ok(json!({ "agents": out, "watchlist": watchlist }))
}

/// Import every discovered session file under the given roots, then the
/// OpenCode SQLite source (if present), then rebuild lineage edges.
pub fn import_all(
    conn: &mut Connection,
    home: &Path,
    roots: &[(&str, PathBuf)],
    opencode_db: Option<&Path>,
    hermes_db: Option<&Path>,
) -> Result<ImportOutcome> {
    // 注意：本函数自身不加锁——锁在 import_with（覆盖 memfiles 等后处理）。
    // 直接调用本函数的测试自行保证单线程；生产路径一律走 import_with。
    let mut out = ImportOutcome {
        files_seen: 0,
        files_updated: 0,
        files_skipped: 0,
        messages_added: 0,
        lines_archived: 0,
        opencode_sessions_updated: 0,
        lineage_links: 0,
        memory_files_monitored: 0,
        memory_revisions_added: 0,
    };
    for (agent, root) in roots {
        let mut files = adapters::discover(root);
        if *agent == adapters::AGENT_PI {
            // 临时现场（scratchpad）整桶不采，见 adapters/pi.rs exclude_temp_buckets
            files = adapters::pi::exclude_temp_buckets(files);
        }
        for path in files {
            out.files_seen += 1;
            let (msgs, lines) = match import_file(conn, home, agent, &path) {
                Ok(r) => r,
                Err(e) if is_file_busy(&e) => {
                    // Windows 默认拒绝读被其他进程独占打开的文件（agent 正在写
                    // 会话文件）：跳过本轮，offset 未推进，下轮增量补采。
                    out.files_skipped += 1;
                    eprintln!("yourmem import: 文件被占用，本轮跳过（下轮补采）：{}", path.display());
                    continue;
                }
                Err(e) => return Err(e),
            };
            if lines > 0 || msgs > 0 {
                out.files_updated += 1;
                out.messages_added += msgs;
                out.lines_archived += lines;
            }
        }
    }
    if let Some(db_path) = opencode_db {
        let oc = adapters::opencode::import(conn, home, db_path)?;
        out.messages_added += oc.messages_added;
        out.lines_archived += oc.lines_archived;
        out.opencode_sessions_updated = oc.sessions_updated;
    }
    if let Some(db_path) = hermes_db {
        let hm = adapters::hermes::import(conn, home, db_path)?;
        out.messages_added += hm.messages_added;
        out.lines_archived += hm.lines_archived;
    }
    out.lineage_links = db::detect_lineage(conn)?;
    Ok(out)
}

/// Import new lines of one file. Returns (messages added, lines archived).
pub fn import_file(conn: &mut Connection, home: &Path, agent: &str, path: &Path) -> Result<(u64, u64)> {
    let path_str = path.to_string_lossy().to_string();
    // 墓碑（v6）：被物理清除过的源文件不再导入——游标已删，放行会完整重导
    // 把被清会话"残缺复活"（codex 七审）。
    {
        let purged: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM purged_sources WHERE path = ?1",
                rusqlite::params![path_str],
                |r| r.get(0),
            )
            .optional()?;
        if purged.is_some() {
            return Ok((0, 0));
        }
    }
    let file_len = std::fs::metadata(path)
        .with_context(|| format!("stat {path_str}"))?
        .len();

    let state = db::source_file_state(conn, agent, &path_str)?;
    let native_id = adapters::native_id(agent, path);
    let session_id = adapters::session_key(agent, &native_id);

    // File replaced / truncated: drop everything derived from it and redo.
    if let Some(s) = &state {
        if file_len < s.imported_bytes {
            db::delete_session_data(conn, &session_id)?;
            db::delete_source_file(conn, agent, &path_str)?;
        }
    }
    let state = db::source_file_state(conn, agent, &path_str)?;
    let (start_offset, start_line) = state
        .as_ref()
        .map(|s| (s.imported_bytes, s.line_count))
        .unwrap_or((0, 0));

    // 已知缺口（DESIGN-0.3 §8 裁定暂缓，V1.0 前再评估）：等长替换检测不到——
    // 文件被重写为相同字节数时 file_len == imported_bytes，会在这里提前返回。
    // JSONL 源是 append-only 模型，等长重写实践中几乎不发生；真要兜底需在
    // source_files 里存尾部行哈希，成本不抵收益。
    if file_len == start_offset {
        // cwd 迟到补查（codex 一审）：首次导入时 state.json 可能尚未写好或读取
        // 失败，此后没有新字节就再也不会进 enrich 分支——会话永久失去项目归属。
        // 只对 kimi：其他 agent 的文件旁边若有同名 state.json 也绝不越权认领
        //（codex 二审复现过 Claude 会话被错挂）。
        if agent == adapters::AGENT_KIMI {
            backfill_kimi_cwd(conn, path, &session_id)?;
        }
        return Ok((0, 0));
    }

    // Read only the new bytes; stop at the last complete line.
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start_offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let complete = match buf.iter().rposition(|b| *b == b'\n') {
        Some(pos) => &buf[..=pos],
        None => return Ok((0, 0)), // no complete line yet
    };
    let new_offset = start_offset + complete.len() as u64;

    // Split into (1-based line_no, raw bytes, lossy text). `complete` always
    // ends with '\n'; strip it first so split() doesn't yield a phantom
    // trailing segment.
    let body = &complete[..complete.len() - 1];
    let mut lines: Vec<(u64, Vec<u8>, String)> = Vec::new();
    let mut line_no = start_line;
    for raw in body.split(|b| *b == b'\n') {
        line_no += 1;
        lines.push((line_no, raw.to_vec(), String::from_utf8_lossy(raw).to_string()));
    }
    if lines.is_empty() {
        return Ok((0, 0));
    }

    let parse_input: Vec<(u64, String)> = lines.iter().map(|(n, _, t)| (*n, t.clone())).collect();
    let prior_saw = state.as_ref().map(|s| s.saw_response_item).unwrap_or(false);
    let parsed = adapters::parse_chunk_with_prior(agent, &parse_input, prior_saw);
    // zcode 的 cwd 不在 rollout 文件里：从它自己的库只读补查（尽力而为）
    let meta = match agent {
        adapters::AGENT_ZCODE => adapters::zcode::enrich_meta_from_db(&parsed.meta, &session_id),
        adapters::AGENT_KIMI => adapters::kimi::enrich_meta_from_state(&parsed.meta, path),
        _ => parsed.meta,
    };

    // 历史行（zcode 的全量快照）出现且该会话已有消息：chunk 内去重不够——
    // 旧消息在早前 chunk 已入账。整文件全量重导一次，重建无重复的消息集
    // （vault_lines 用 OR REPLACE 幂等；offset 照常推进，不影响增量）。
    let (messages, lines, resync, artifacts, uuids, compact_line) = if parsed.history_resync && start_offset > 0 {
        // 字节保真重读（codex 复审：read_to_string().lines() 会剥 CRLF 的 \r、
        // 跳过空行，再 OR REPLACE 会永久污染 vault manifest）。这里完全镜像
        // 正常路径：原始字节、截到最后一个换行（未换行尾部照旧留到下一轮）、
        // 不跳空行（行号与正常路径一致）。
        let mut buf = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
        // 严格截到第一次增量读取的边界（codex 三审：两次读取之间 agent 又
        // 追加了完整行的话，全量重读会提前入账而 offset 没跟上，下一轮双导）。
        let snap_len = (new_offset as usize).min(buf.len());
        let snap = &buf[..snap_len];
        let Some(pos) = snap.iter().rposition(|b| *b == b'\n') else {
            return Ok((0, 0));
        };
        let body = &snap[..pos]; // 不含末换行，与正常路径的 split 口径一致
        let mut full: Vec<(u64, Vec<u8>, String)> = Vec::new();
        let mut n = 0u64;
        for raw in body.split(|b| *b == b'\n') {
            n += 1;
            full.push((n, raw.to_vec(), String::from_utf8_lossy(raw).to_string()));
        }
        let full_input: Vec<(u64, String)> = full.iter().map(|(n, _, t)| (*n, t.clone())).collect();
        let full_parsed = adapters::parse_chunk(agent, &full_input);
        (full_parsed.messages, full, true, full_parsed.artifacts, full_parsed.uuids, full_parsed.compact_line)
    } else {
        (parsed.messages, lines, false, parsed.artifacts, parsed.uuids, parsed.compact_line)
    };

    let project_id = match meta.cwd.as_deref() {
        Some(cwd) => {
            let (root, name) = db::project_root_for(cwd);
            Some(db::upsert_project(conn, &root, &name)?)
        }
        None => None,
    };

    // One transaction: messages + vault manifest + offset. Vault objects are
    // written to disk first; they are content-addressed and immutable, so a
    // crash here only risks a few orphan objects, never DB inconsistency.
    // message_count 一律"插入后绝对重算"（codex 三审方向）：OR IGNORE 之后的
    // 真实行数就是真相，累加语义在重放/并发/清理场景全都容易虚。
    let old_count: i64 = conn.query_row(
        "SELECT message_count FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
        |r| r.get(0),
    )
    .unwrap_or(0);

    let old_vault_hashes: Vec<String> = if resync {
        let mut stmt = conn.prepare("SELECT DISTINCT hash FROM vault_lines WHERE session_id = ?1")?;
        let rows = stmt.query_map(rusqlite::params![session_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    } else {
        Vec::new()
    };

    let tx = conn.transaction()?;

    db::upsert_session(
        &tx,
        &session_id,
        agent,
        &native_id,
        project_id,
        &path_str,
        &meta,
        0,
    )?;
    if resync {
        // messages rowid 删除后会被复用：先断开指向本会话消息的来源指针，
        // 否则重导后指针可能错挂到无关消息（同 delete_session_data 的处理）
        tx.execute(
            "UPDATE memories SET source_message_id = NULL WHERE source_session_id = ?1 AND source_message_id IS NOT NULL",
            rusqlite::params![session_id],
        )?;
        tx.execute("DELETE FROM messages WHERE session_id = ?1", rusqlite::params![session_id])?;
        tx.execute("DELETE FROM vault_lines WHERE session_id = ?1", rusqlite::params![session_id])?;
        // 压缩点随全量重导重建：先清（文件被截断替换时旧边界行可能已不存在）
        tx.execute(
            "UPDATE sessions SET compact_line_no = NULL WHERE id = ?1",
            rusqlite::params![session_id],
        )?;
    }
    db::insert_messages(&tx, &session_id, &messages)?;
    tx.execute(
        "UPDATE sessions SET message_count = (SELECT COUNT(*) FROM messages WHERE session_id = ?1) WHERE id = ?1",
        rusqlite::params![session_id],
    )?;
    if let Some(cl) = compact_line {
        // 压缩点取 MIN（0.3.8）：多次压缩保持首次边界；重导清空后无条件落新值
        tx.execute(
            "UPDATE sessions SET compact_line_no = ?2 WHERE id = ?1 AND (compact_line_no IS NULL OR compact_line_no > ?2)",
            rusqlite::params![session_id, cl],
        )?;
    }
    if resync {
        // 重导时 artifacts/uuids 同样按整文件重建（先清后插，幂等）
        tx.execute("DELETE FROM session_artifacts WHERE session_id = ?1", rusqlite::params![session_id])?;
        tx.execute("DELETE FROM session_uuids WHERE session_id = ?1", rusqlite::params![session_id])?;
    }
    db::insert_artifacts(&tx, &session_id, project_id, &artifacts)?;
    db::insert_uuid_sightings(&tx, &session_id, &uuids)?;
    // kimi 旧 artifact 的空项目归属无条件修复（codex 三审）：早前无 state.json
    // 时入账的 artifact project_id 为 NULL，本块新 artifact 有归属而旧的可能
    // 没有；回填分支的 cwd IS NULL 门槛过后就再无人修它。与会话同事务。
    if agent == adapters::AGENT_KIMI {
        if let Some(pid) = project_id {
            tx.execute(
                "UPDATE session_artifacts SET project_id = ?1 WHERE session_id = ?2 AND project_id IS NULL",
                rusqlite::params![pid, session_id],
            )?;
        }
    }

    for (n, raw_bytes, _) in &lines {
        let hash = vault::store_line(home, raw_bytes)?;
        tx.execute(
            "INSERT OR REPLACE INTO vault_lines(session_id, line_no, hash) VALUES (?1,?2,?3)",
            rusqlite::params![session_id, *n as i64, hash],
        )?;
    }

    // 净新增在事务内算好（commit 会消耗 tx）
    let total: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
        rusqlite::params![session_id],
        |r| r.get(0),
    )?;
    db::update_source_file(&tx, &path_str, agent, new_offset, line_no, None)?;
    // codex 新格式标志持久化：行必须在 update_source_file 建好之后更新；
    // 只置 1 不清零（老格式转新格式是单向的，见 codex adapter）
    if parsed.saw_response_items {
        tx.execute(
            "UPDATE source_files SET saw_response_item = 1 WHERE agent = ?1 AND path = ?2",
            rusqlite::params![agent, path_str],
        )?;
    }
    tx.commit()?;
    for hash in old_vault_hashes {
        let referenced: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM vault_lines WHERE hash=?1) OR EXISTS(SELECT 1 FROM memory_revisions WHERE hash=?1)",
            rusqlite::params![hash],
            |r| r.get(0),
        )?;
        if referenced == 0 {
            let _ = std::fs::remove_file(vault::object_path(home, &hash));
        }
    }

    let net = (total - old_count).max(0) as u64;
    // resync 分支的 lines 就是全量重读的 full——两种情况下"本轮归档了多少行"
    // 都是 lines.len()；此前 resync 报 chunk_lines 会少报（自检 A8，仅统计口径）
    let archived = lines.len() as u64;
    Ok((net, archived))
}

pub fn outcome_json(o: &ImportOutcome) -> Value {
    json!({
        "files_seen": o.files_seen,
        "files_updated": o.files_updated,
        "files_skipped": o.files_skipped,
        "opencode_sessions_updated": o.opencode_sessions_updated,
        "messages_added": o.messages_added,
        "lines_archived": o.lines_archived,
        "lineage_links": o.lineage_links,
        "memory_files_monitored": o.memory_files_monitored,
        "memory_revisions_added": o.memory_revisions_added,
    })
}

/// 错误链里是否为文件读取被占用/被拒（Windows 文件占用锁的信号；真机实测
/// 2026-09-03：独占写打开的文件报 os error 32/33（ERROR_SHARING_VIOLATION /
/// ERROR_LOCK_VIOLATION），不是 error 5 的 PermissionDenied——两者都要接住）。
/// raw_os_error 数值平台相关，Windows 码值只在 Windows 分支比对；Unix 上
/// 读文件几乎不会 PermissionDenied，行为不变。
fn is_file_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<std::io::Error>() {
        Some(io) => {
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
            #[cfg(target_os = "windows")]
            {
                matches!(io.raw_os_error(), Some(32) | Some(33))
            }
            #[cfg(not(target_os = "windows"))]
            {
                false
            }
        }
        None => false,
    })
}

/// 跨进程导入互斥（四审定稿）：不复刻文件锁的 staleness/令牌/心跳一堆竞态，
/// 直接用 SQLite 的 BEGIN IMMEDIATE——独立锁库（.import.lock.db），显式写
/// 事务天然跨进程互斥：拿不到就忙等 busy_timeout，超时 Err（fail-closed）；
/// 进程崩溃连接即断、锁即时释放，无残留清理、无所有权竞态。
pub struct ImportLockTx {
    conn: Option<rusqlite::Connection>,
}

impl ImportLockTx {
    pub fn acquire(home: &Path, max_wait: std::time::Duration) -> Result<ImportLockTx> {
        let lock_db = home.join(".import.lock.db");
        let conn = rusqlite::Connection::open(&lock_db)?;
        conn.busy_timeout(max_wait)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS import_lock(x INTEGER)",
        )?;
        // BEGIN IMMEDIATE 本身就是写锁；连接保持存活即持锁
        conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(ImportLockTx { conn: Some(conn) })
    }
}

impl Drop for ImportLockTx {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let _ = c.execute_batch("ROLLBACK"); // 释放写锁；连接随后关闭
        }
    }
}

/// kimi 会话的 cwd 迟到补查：会话已存在但 cwd 为空且 state.json 现在能读到。
fn backfill_kimi_cwd(conn: &Connection, path: &Path, session_id: &str) -> Result<()> {
    let missing: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1 AND cwd IS NULL",
            rusqlite::params![session_id],
            |r| r.get(0),
        )
        .optional()?;
    if missing.is_none() {
        return Ok(());
    }
    let meta = adapters::kimi::enrich_meta_from_state(&crate::models::SessionMetaPatch::default(), path);
    if let Some(cwd) = meta.cwd {
        // 会话与 artifact 同一事务（codex 三审：两条自动提交语句中途失败会让
        // cwd 已写、artifact 仍空，且 cwd IS NULL 门槛让重试永久跳过）
        let tx = conn.unchecked_transaction()?;
        let (root, name) = db::project_root_for(&cwd);
        let pid = db::upsert_project(&tx, &root, &name)?;
        tx.execute(
            "UPDATE sessions SET cwd = ?1, project_id = ?2 WHERE id = ?3",
            rusqlite::params![cwd, pid, session_id],
        )?;
        // 既有 artifact 的 project_id 一并归属（按 artifact 自身 project_id
        // 过滤的查询才能查到；OR IGNORE 的重放不会修正旧记录——codex 二审）
        tx.execute(
            "UPDATE session_artifacts SET project_id = ?1 WHERE session_id = ?2 AND project_id IS NULL",
            rusqlite::params![pid, session_id],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_two_agents_both_collected() {
        // v8 主键 (agent, path) 的行为锚：同一路径给两个 agent 时各自持游标，
        // 后一个不再被第一个的游标顶掉（codex 评审 blocker——单 path 主键下
        // 第二个 agent 直接命中 file_len == imported_bytes 整体漏采）
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aaaa-9999.jsonl"),
            format!("{}\n", r#"{"type":"user","cwd":"/tmp/dual-agent","uuid":"x1","timestamp":"2026-08-29T09:00:00Z","message":{"role":"user","content":"双 agent 同路径"}}"#),
        )
        .unwrap();
        let mut conn = crate::db::open(home.path()).unwrap();
        let roots = vec![
            (crate::adapters::AGENT_CLAUDE, dir.path().to_path_buf()),
            (crate::adapters::AGENT_CODEX, dir.path().to_path_buf()),
        ];
        let out = import_all(&mut conn, home.path(), &roots, None, None).unwrap();
        assert_eq!(out.files_seen, 2, "discover 对两个 agent 各跑一遍");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT agent) FROM sessions WHERE id LIKE '%aaaa-9999%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "两个 agent 都要建会话，第二个不能被游标顶掉");
        // 再导一轮：各自游标独立，双双无新增
        let again = import_all(&mut conn, home.path(), &roots, None, None).unwrap();
        assert_eq!(again.messages_added, 0);
    }

    #[test]
    fn extra_root_dedup_key_is_agent_plus_path() {
        // 同一路径不同 agent 并存（各自解析）；完全相同的 (agent, path) 才去重
        let mut roots: Vec<(String, PathBuf)> = vec![("claude".into(), PathBuf::from("/a"))];
        push_extra_root(&mut roots, "kimi".into(), PathBuf::from("/a"));
        push_extra_root(&mut roots, "claude".into(), PathBuf::from("/a"));
        push_extra_root(&mut roots, "claude".into(), PathBuf::from("/b"));
        let summary: Vec<(&str, &str)> = roots.iter().map(|(a, p)| (a.as_str(), p.to_str().unwrap())).collect();
        assert_eq!(summary, vec![
            ("claude", "/a"),
            ("kimi", "/a"),
            ("claude", "/b"),
        ], "按 path 去重会吞掉 kimi 的同路径根（codex 评审 blocker）");
    }

    #[test]
    fn import_lock_excludes_and_releases() {
        let home = tempfile::tempdir().unwrap();
        // 占锁
        let g = ImportLockTx::acquire(home.path(), std::time::Duration::from_millis(200)).unwrap();
        // 第二个获取者：短 busy_timeout 必须 Err（fail-closed）
        let r = ImportLockTx::acquire(home.path(), std::time::Duration::from_millis(150));
        assert!(r.is_err(), "锁被占时必须报错");
        // 释放后可立即获取（drop 回滚写事务，锁即时释放——无残留清理）
        drop(g);
        assert!(ImportLockTx::acquire(home.path(), std::time::Duration::from_millis(200)).is_ok());
    }
}
