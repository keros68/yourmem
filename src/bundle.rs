//! Bundle: one-shot migration backup & restore (DESIGN-0.3 §5). 搬家，不是同步。
//!
//! Layout inside the `.tar.gz`:
//!   <stem>/manifest.json        — format/app/schema version, object counts, filter
//!   <stem>/db/<ts>.sqlite       — VACUUM INTO consistency snapshot
//!   <stem>/objects/<shard>/<hash> — CAS objects, verbatim copies
//!
//! Fidelity rule: objects are content-addressed, `verify` re-hashes every one
//! against its address; the DB snapshot's `PRAGMA user_version` is the
//! schema_version recorded in the manifest.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};

use crate::{db, vault};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Default, Clone)]
pub struct BundleFilter {
    pub agent: Option<String>,
    pub project: Option<String>,
}

impl BundleFilter {
    pub fn is_empty(&self) -> bool {
        self.agent.is_none() && self.project.is_none()
    }
}

/// Create a bundle at `out` (a .tar.gz path). Returns the manifest.
pub fn create(conn: &Connection, home: &Path, out: &Path, filter: &BundleFilter) -> Result<Value> {
    let staging = tempfile::tempdir().context("create staging dir")?;
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bundle".to_string());
    // strip a trailing ".tar" so x.tar.gz -> x
    let stem = stem.strip_suffix(".tar").map(str::to_string).unwrap_or(stem);
    let root = staging.path().join(&stem);
    std::fs::create_dir_all(root.join("db"))?;
    std::fs::create_dir_all(root.join("objects"))?;

    // 1. Consistent DB snapshot (VACUUM INTO works on a live DB).
    let snap_name = format!("yourmem-{}.sqlite", chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f"));
    let snap_path = root.join("db").join(&snap_name);
    conn.execute("VACUUM INTO ?1", params![snap_path.to_string_lossy().as_ref()])?;

    // 2. Optional filter: prune the snapshot (never the live DB).
    let mut filter_report = Value::Null;
    if !filter.is_empty() {
        filter_report = prune_snapshot(&snap_path, filter)?;
    }

    // 3. Copy only the objects still referenced by the (pruned) snapshot.
    let snap = Connection::open_with_flags(&snap_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let keep = referenced_hashes(&snap)?;
    let schema_version: i32 = snap.pragma_query_value(None, "user_version", |r| r.get(0))?;
    drop(snap);

    let mut objects = 0u64;
    let mut bytes = 0u64;
    let mut missing = 0u64;
    for hash in &keep {
        let src = vault::object_path(home, hash);
        if !src.is_file() {
            missing += 1;
            continue;
        }
        let dst = root.join("objects").join(hash.get(..2).unwrap_or(hash)).join(hash);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::copy(&src, &dst)?;
        objects += 1;
        bytes += std::fs::metadata(&dst)?.len();
    }
    // 保真是"验"出来的：引用的对象缺一个，恢复出的库就有导不出来的会话，
    // 这种 bundle 不该被创建（宁可失败也不打包一个自洽但残缺的包）。
    anyhow::ensure!(
        missing == 0,
        "vault 缺 {missing} 个被引用的对象，bundle 未创建——先跑 backup status 检查对象库"
    );

    let manifest = json!({
        "format_version": FORMAT_VERSION,
        "app_version": env!("CARGO_PKG_VERSION"),
        "schema_version": schema_version,
        "created_at": crate::now_iso(),
        "db_snapshot": snap_name,
        "objects": objects,
        "objects_bytes": bytes,
        "filter": filter_report,
    });
    std::fs::write(root.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;

    // 4. Pack tar.gz.
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(out)?;
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(&stem, &root)?;
    tar.into_inner()?.finish()?;
    Ok(manifest)
}

/// Delete everything in the snapshot DB that doesn't match the filter.
/// 删除顺序遵守外键：delete_session_data 连带子表（messages/vault_lines/
/// session_uuids）→ 依赖会话的小表 → handoffs/memories/artifacts → projects；
/// memory_revisions → memory_files。
fn prune_snapshot(snap_path: &Path, filter: &BundleFilter) -> Result<Value> {
    let conn = Connection::open(snap_path)?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;

    // 1. 要保留的会话集合（agent 与 project 过滤取交集）
    let mut keep_sessions: Option<HashSet<String>> = None;
    if let Some(agent) = &filter.agent {
        let mut stmt = conn.prepare("SELECT id FROM sessions WHERE agent = ?1")?;
        let ids = stmt
            .query_map(params![agent], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        keep_sessions = Some(ids);
    }
    if let Some(proj) = &filter.project {
        let mut stmt = conn.prepare(
            "SELECT s.id FROM sessions s JOIN projects p ON p.id = s.project_id
             WHERE p.name = ?1 OR p.path LIKE '%'||?1||'%'",
        )?;
        let ids = stmt
            .query_map(params![proj], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        keep_sessions = Some(match keep_sessions {
            Some(existing) => existing.intersection(&ids).cloned().collect(),
            None => ids,
        });
    }

    // 2. 删会话及其派生（FTS 由触发器维护）
    let mut pruned = 0u64;
    if let Some(keep) = &keep_sessions {
        let mut stmt = conn.prepare("SELECT id, agent, file_path FROM sessions")?;
        let all: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (sid, agent, fpath) in all {
            if !keep.contains(&sid) {
                // 文件型 agent 1 文件=1 会话：一并裁掉该文件的采集游标，否则恢复端
                // import 会跳过这些字节，被裁会话永远采不回且无提示。opencode 是
                // 整库共享游标，删了会让被裁会话在带源库的机器上整库重扫复活——
                // 有意保留（静默少采比复活安全，墓碑对 SQLite agent 本就不生效）。
                if agent != crate::adapters::opencode::AGENT_OPENCODE {
                    conn.execute(
                        "DELETE FROM source_files WHERE agent = ?1 AND path = ?2",
                        params![agent, fpath],
                    )?;
                }
                db::delete_session_data(&conn, &sid)?;
                pruned += 1;
            }
        }
        conn.execute(
            "DELETE FROM session_links WHERE child_session_id NOT IN (SELECT id FROM sessions)
                OR parent_session_id NOT IN (SELECT id FROM sessions)",
            [],
        )?;
        conn.execute(
            "DELETE FROM session_artifacts WHERE session_id NOT IN (SELECT id FROM sessions)",
            [],
        )?;
    }

    // 3. 项目过滤：会话清完后再清项目派生物，最后 projects 本身
    if let Some(proj) = &filter.project {
        let keep_proj = "SELECT id FROM projects WHERE name = ?1 OR path LIKE '%'||?1||'%'";
        conn.execute(
            &format!("DELETE FROM handoffs WHERE project_id NOT IN ({keep_proj})"),
            params![proj],
        )?;
        conn.execute(
            &format!("DELETE FROM memories WHERE project_id IS NOT NULL AND project_id NOT IN ({keep_proj})"),
            params![proj],
        )?;
        conn.execute(
            &format!("DELETE FROM session_artifacts WHERE project_id IS NOT NULL AND project_id NOT IN ({keep_proj})"),
            params![proj],
        )?;
        conn.execute(
            "DELETE FROM projects WHERE NOT (name = ?1 OR path LIKE '%'||?1||'%')",
            params![proj],
        )?;
    }

    // 3.5 墓碑随过滤裁剪（codex 九/十/十一审）：
    // - agent 过滤：只留本 agent 墓碑（他 agent 墓碑会阻断目标库导入）；
    // - project 过滤：全删。被清会话已不在快照、文件型墓碑按 path 阻断
    //   （与 session id 无关），且 claude 的会话目录是项目路径哈希、无从
    //   反查归属——多带的墓碑会静默阻断他项目的本地导入（codex 复现），
    //   漏采比"可能复活"（可再清）更糟。
    // - 不能做"会话没了墓碑也删"的孤儿清理：purge 语义下墓碑与会话互斥
    //   存在（先删会话后写墓碑），NOT IN sessions 会误删全部有效墓碑。
    //   残余风险记档：project 过滤 bundle 恢复后，被清会话可能经本地源
    //   文件复活，需重新 purge。
    if filter.project.is_some() {
        conn.execute("DELETE FROM purged_sources", [])?;
    } else if let Some(agent) = &filter.agent {
        conn.execute(
            "DELETE FROM purged_sources WHERE session_id NOT LIKE ?1 || ':%'",
            params![agent],
        )?;
    }

    // 4. agent 过滤：原生 memory 文件按 agent 裁剪（先 FTS 再 revisions 后 files；
    //    memory_fts 无触发器手动维护，漏裁会留孤儿行，rowid 复用后串搜索结果）
    if let Some(agent) = &filter.agent {
        conn.execute(
            "DELETE FROM memory_fts WHERE rowid IN (SELECT id FROM memory_files WHERE agent != ?1)",
            params![agent],
        )?;
        conn.execute(
            "DELETE FROM memory_revisions WHERE file_id IN (SELECT id FROM memory_files WHERE agent != ?1)",
            params![agent],
        )?;
        conn.execute("DELETE FROM memory_files WHERE agent != ?1", params![agent])?;
    }
    Ok(json!({ "agent": filter.agent, "project": filter.project, "sessions_pruned": pruned }))
}

/// Hashes still referenced by a (possibly pruned) snapshot.
fn referenced_hashes(conn: &Connection) -> Result<HashSet<String>> {
    let mut keep = HashSet::new();
    for sql in ["SELECT DISTINCT hash FROM vault_lines", "SELECT DISTINCT hash FROM memory_revisions"] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        keep.extend(rows);
    }
    Ok(keep)
}

struct BundleLayout {
    root: PathBuf,         // extracted <stem>/ dir
    manifest: Value,
}

fn extract(bundle: &Path) -> Result<(tempfile::TempDir, BundleLayout)> {
    let tmp = tempfile::tempdir().context("extract staging")?;
    let file = std::fs::File::open(bundle).with_context(|| format!("open {}", bundle.display()))?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(dec);
    ar.unpack(tmp.path())?;
    // Locate manifest.json (one top-level dir deep by construction, but be lenient).
    let mut found = None;
    for entry in std::fs::read_dir(tmp.path())? {
        let m = entry?.path().join("manifest.json");
        if m.is_file() {
            found = Some(m);
            break;
        }
    }
    let manifest_path = found.with_context(|| "bundle 里没有 manifest.json")?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    anyhow::ensure!(
        manifest["format_version"].as_u64() == Some(FORMAT_VERSION as u64),
        "unsupported bundle format_version: {}",
        manifest["format_version"]
    );
    let root = manifest_path.parent().unwrap().to_path_buf();
    Ok((tmp, BundleLayout { root, manifest }))
}

/// Verify a bundle: re-hash every object against its content address and
/// reconcile counts/bytes with the manifest; check the DB snapshot opens.
pub fn verify(bundle: &Path) -> Result<Value> {
    let (_tmp, layout) = extract(bundle)?;
    let objects_dir = layout.root.join("objects");
    let mut objects = 0u64;
    let mut bytes = 0u64;
    let mut corrupted: Vec<String> = Vec::new();
    if objects_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&objects_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let hash = entry.file_name().to_string_lossy().to_string();
            let data = std::fs::read(entry.path())?;
            objects += 1;
            bytes += data.len() as u64;
            if vault::hash_bytes(&data) != hash {
                corrupted.push(hash);
            }
        }
    }
    let db_file = layout.root.join("db").join(layout.manifest["db_snapshot"].as_str().unwrap_or_default());
    let db_opens = Connection::open_with_flags(&db_file, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| c.pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0)))
        .is_ok();

    let expected = &layout.manifest;
    let ok = corrupted.is_empty()
        && objects == expected["objects"].as_u64().unwrap_or(u64::MAX)
        && bytes == expected["objects_bytes"].as_u64().unwrap_or(u64::MAX)
        && db_opens;
    Ok(json!({
        "ok": ok,
        "objects": objects,
        "objects_bytes": bytes,
        "corrupted": corrupted,
        "db_snapshot_opens": db_opens,
        "manifest": layout.manifest,
    }))
}

/// Restore a bundle into `target_home`. Default: fresh home (换电脑场景);
/// `--merge`: merge into an existing library.
pub fn restore(bundle: &Path, target_home: &Path, merge: bool) -> Result<Value> {
    let report = verify(bundle)?;
    anyhow::ensure!(report["ok"] == true, "bundle 校验失败：{}", serde_json::to_string_pretty(&report)?);
    // 合并只信自己库的 schema（AGENTS.md）：更新版本产出的 bundle 直接拒绝，
    // 不让列不匹配退化成半路崩出的 SQL 错误。
    let bundle_schema = report["manifest"]["schema_version"].as_i64().unwrap_or(i64::MAX);
    anyhow::ensure!(
        bundle_schema <= i64::from(db::SCHEMA_VERSION),
        "bundle schema_version {bundle_schema} 比本程序（{}）新——请先升级 yourmem",
        db::SCHEMA_VERSION
    );
    let (_tmp, layout) = extract(bundle)?;
    let src_db_path = layout.root.join("db").join(layout.manifest["db_snapshot"].as_str().unwrap());

    if !merge {
        let target_db = target_home.join("yourmem.db");
        anyhow::ensure!(
            !target_db.exists(),
            "{} 已存在——恢复到全新目录，或加 --merge 合并",
            target_db.display()
        );
        std::fs::create_dir_all(target_home.join("objects"))?;
        std::fs::copy(&src_db_path, &target_db)?;
        let copied = copy_objects(&layout.root, target_home)?;
        return Ok(json!({
            "mode": "fresh", "home": target_home, "objects_copied": copied,
            "note": "一次性迁移完成；这不是同步，源机器的后续变化不会自动过来。",
        }));
    }

    anyhow::ensure!(target_home.join("yourmem.db").is_file(), "--merge 需要目标已有 yourmem 库");
    let conn = db::open(target_home)?;
    let merged = merge_db(&conn, &src_db_path)?;
    let copied = copy_objects(&layout.root, target_home)?;
    // 并入的原生 memory 文件立即可搜（自检 B3 + codex 评审 blocker）：memory_fts
    // 无触发器、内容在 vault 对象里——merge 不带 fts 行，等对象拷完按最新修订
    // 回填；merge 更新过 current_hash 的文件强制重建（旧行是旧内容）
    let stale: Vec<i64> = merged["memory_fts_stale"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let fts_backfilled = backfill_memory_fts(&conn, target_home, &stale)?;
    Ok(json!({
        "mode": "merge", "home": target_home, "objects_copied": copied, "merged": merged,
        "memory_fts_backfilled": fts_backfilled,
        "note": "一次性合并完成；这不是同步，源机器的后续变化不会自动过来。",
    }))
}

/// Copy bundle objects into a home's CAS store. Content-addressed: existing
/// objects are skipped untouched (幂等).
fn copy_objects(bundle_root: &Path, target_home: &Path) -> Result<u64> {
    let mut copied = 0u64;
    let objects_dir = bundle_root.join("objects");
    if !objects_dir.is_dir() {
        return Ok(0);
    }
    for entry in walkdir::WalkDir::new(&objects_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let hash = entry.file_name().to_string_lossy().to_string();
        let dst = vault::object_path(target_home, &hash);
        if dst.exists() {
            continue;
        }
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::copy(entry.path(), &dst)?;
        copied += 1;
    }
    Ok(copied)
}

/// Merge a bundle's DB snapshot into an existing library (ATTACH + upserts).
///
/// 规则：
/// - projects 按 path 合并（记录 id 重映射，所有带 project_id 的表一律走它）；
/// - session 已存在且 bundle 侧不更新：整体跳过；bundle 更新（消息更多）：
///   该会话整体替换（消息重插拿新 rowid，FTS 由触发器维护）；
/// - 小表按主键/唯一约束 INSERT OR IGNORE；source_files offset 取 max；
/// - memory_revisions 无唯一约束，按 (file_id, hash) 查重后插入（file_id 重映射）。
/// 给缺 memory_fts 行的原生 memory 文件按最新修订回填索引（内容在 vault 对象里）。
/// `force` 里的文件即使已有 fts 行也强制重建（merge 更新过 current_hash，旧行是
/// 旧内容）。对象缺失/非 UTF-8 跳过——与 memfiles collect 的"留到下一轮"口径
/// 一致；返回实际（重）建了索引的条数。
fn backfill_memory_fts(conn: &Connection, home: &Path, force: &[i64]) -> Result<u64> {
    let mut targets: Vec<(i64, Option<String>)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT f.id, (SELECT r.hash FROM memory_revisions r
                           WHERE r.file_id = f.id ORDER BY r.id DESC LIMIT 1)
             FROM memory_files f
             WHERE NOT EXISTS (SELECT 1 FROM memory_fts WHERE rowid = f.id)",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        targets.extend(rows);
    }
    for &fid in force {
        let hash: Option<String> = conn
            .query_row(
                "SELECT hash FROM memory_revisions WHERE file_id = ?1 ORDER BY id DESC LIMIT 1",
                params![fid],
                |r| r.get(0),
            )
            .optional()?;
        targets.push((fid, hash));
    }
    let mut done = 0u64;
    for (fid, hash) in targets {
        let Some(hash) = hash else { continue };
        match vault::read_object(home, &hash, false) {
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    // 索引失败不再无声吞掉（codex 评审）：数据已合并、索引是派生物，
                    // 不阻断恢复，但必须让人看见
                    if let Err(e) = db::set_memory_fts(conn, fid, text) {
                        eprintln!("yourmem bundle: memory_fts 回填失败 file {fid}: {e:#}");
                        continue;
                    }
                    done += 1;
                }
                Err(_) => continue,
            },
            Err(_) => continue,
        }
    }
    Ok(done)
}

fn merge_db(target: &Connection, src_db: &Path) -> Result<Value> {
    target.execute("ATTACH DATABASE ?1 AS src", params![src_db.to_string_lossy().as_ref()])?;
    let r = (|| -> Result<Value> {
        let tx = target.unchecked_transaction()?;

        // projects: merge by path, build old->new id map
        let mut project_map: HashMap<i64, i64> = HashMap::new();
        {
            // archived_at 是 schema v11 的列；更早的 bundle 没有它，按 NULL（活跃）并入。
            // src 有值时并入目标且 MAX 取新：任何一侧已归档都保持归档——merge 不复活。
            let arch_col = {
                let mut st = tx.prepare("PRAGMA src.table_info(projects)")?;
                let cols = st.query_map([], |r| r.get::<_, String>(1))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                if cols.iter().any(|c| c == "archived_at") { "archived_at" } else { "NULL" }
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT id, path, name, created_at, updated_at, {arch_col} FROM src.projects"
            ))?;
            let rows: Vec<(i64, String, String, String, String, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (old_id, path, name, created, _updated, archived_at) in rows {
                let new_id = db::upsert_project(&tx, &path, &name)?;
                if let Some(ts) = archived_at {
                    tx.execute(
                        "UPDATE projects SET archived_at = MAX(COALESCE(archived_at, ''), ?1) WHERE id = ?2",
                        params![ts, new_id],
                    )?;
                }
                let _ = created;
                project_map.insert(old_id, new_id);
            }
        }

        // sessions: skip when target's copy is at least as new, else replace wholesale
        let mut sessions_added = 0u64;
        // 被替换会话上既有 memory 指针的坐标（memory id, 旧消息 id, sid, line, ord）
        // ——按 memory id（TEXT，mem_ 前缀）记录：旧消息 rowid 会被重插复用，
        // 按它做 UPDATE 选择条件会连环错挂（终审三轮）
        let mut replaced_pointer_coords: Vec<(String, i64, String, i64, i64)> = Vec::new();
        let mut sessions_replaced = 0u64;
        let mut sessions_skipped = 0u64;
        let mut replaced_ids: Vec<String> = Vec::new();
        {
            // deleted_at 是 schema v4 的列；v3 bundle 没有它，按 NULL（未删除）并入。
            // compact_line_no 是 v10 的列；更早的 bundle 没有它，按 NULL（无压缩点）并入。
            let (del_col, compact_col) = {
                let mut st = tx.prepare("PRAGMA src.table_info(sessions)")?;
                let cols = st.query_map([], |r| r.get::<_, String>(1))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                (
                    if cols.iter().any(|c| c == "deleted_at") { "deleted_at" } else { "NULL" },
                    if cols.iter().any(|c| c == "compact_line_no") { "compact_line_no" } else { "NULL" },
                )
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT id, agent, native_id, project_id, file_path, cwd, git_branch,
                        started_at, ended_at, message_count, first_parent_uuid, compact_leaf_uuid,
                        created_at, updated_at, {del_col}, {compact_col} FROM src.sessions",
            ))?;
            let sessions: Vec<Value> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?, "agent": r.get::<_, String>(1)?,
                        "native_id": r.get::<_, String>(2)?, "project_id": r.get::<_, Option<i64>>(3)?,
                        "file_path": r.get::<_, String>(4)?, "cwd": r.get::<_, Option<String>>(5)?,
                        "git_branch": r.get::<_, Option<String>>(6)?, "started_at": r.get::<_, Option<String>>(7)?,
                        "ended_at": r.get::<_, Option<String>>(8)?, "message_count": r.get::<_, i64>(9)?,
                        "first_parent_uuid": r.get::<_, Option<String>>(10)?, "compact_leaf_uuid": r.get::<_, Option<String>>(11)?,
                        "created_at": r.get::<_, String>(12)?, "updated_at": r.get::<_, String>(13)?,
                        "deleted_at": r.get::<_, Option<String>>(14)?,
                        "compact_line_no": r.get::<_, Option<i64>>(15)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for s in sessions {
                let sid = s["id"].as_str().unwrap();
                let existing_mc: Option<i64> = tx
                    .query_row("SELECT message_count FROM sessions WHERE id = ?1", params![sid], |r| r.get(0))
                    .optional()?;
                match existing_mc {
                    Some(mc) if mc >= s["message_count"].as_i64().unwrap_or(0) => {
                        sessions_skipped += 1;
                    }
                    Some(_) => {
                        // 替换前抓目标库既有 memory 的来源指针坐标（终审二轮）
                        let coords: Vec<(String, i64, String, i64, i64)> = {
                            let mut st = tx.prepare(
                                "SELECT mm.id, m.id, m.session_id, m.line_no, m.ord
                                 FROM memories mm JOIN messages m ON mm.source_message_id = m.id
                                 WHERE m.session_id = ?1",
                            )?;
                            let rows = st.query_map(params![sid], |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                            })?;
                            rows.collect::<std::result::Result<Vec<_>, _>>()?
                        };
                        replaced_pointer_coords.extend(coords);
                        db::delete_session_data(&tx, sid)?;
                        insert_session(&tx, &s, &project_map)?;
                        sessions_replaced += 1;
                        replaced_ids.push(sid.to_string());
                    }
                    None => {
                        insert_session(&tx, &s, &project_map)?;
                        sessions_added += 1;
                        replaced_ids.push(sid.to_string());
                    }
                }
            }
        }

        // messages + vault_lines + uuids for newly added/replaced sessions
        // 外来 bundle 的哈希先做 64-hex 门检（自检 B2 + codex 评审 blocker）：
        // vault_lines 直插会在 export 校验时才炸；memory_revisions 的坏短哈希
        // 更糟——回填 FTS 时 object_path 的 hash[..2] 直接 panic，且此时 merge
        // 已提交。fail-closed 拒合并。
        {
            let bad_vl: i64 = tx.query_row(
                "SELECT COUNT(*) FROM src.vault_lines
                 WHERE length(hash) != 64 OR hash GLOB '*[^0-9a-f]*'",
                [],
                |r| r.get(0),
            )?;
            anyhow::ensure!(bad_vl == 0, "bundle 内有 {bad_vl} 行 vault_lines 哈希非 64 位小写 hex，拒绝合并（外来包不可信）");
            let bad_mr: i64 = tx.query_row(
                "SELECT COUNT(*) FROM src.memory_revisions
                 WHERE length(hash) != 64 OR hash GLOB '*[^0-9a-f]*'",
                [],
                |r| r.get(0),
            )?;
            anyhow::ensure!(bad_mr == 0, "bundle 内有 {bad_mr} 行 memory_revisions 哈希非 64 位小写 hex，拒绝合并（外来包不可信）");
        }
        let mut msg_id_map: HashMap<i64, i64> = HashMap::new();
        // skipped 会话：src 消息坐标 → 目标库现有行 id（终审二轮：只映射新
        // 会话会让 skipped 的 memory 指针静默变 NULL）
        {
            let mut st = tx.prepare(
                "SELECT sm.id, tm.id FROM src.messages sm
                 JOIN messages tm ON tm.session_id = sm.session_id
                     AND tm.line_no = sm.line_no AND tm.ord = sm.ord",
            )?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for (a, b) in rows.collect::<std::result::Result<Vec<_>, _>>()? {
                msg_id_map.entry(a).or_insert(b);
            }
        }
        // （kind 按原文直插，不走 enum 往返——合并的首要原则是保真）
        let mut messages_added = 0u64;
        for sid in &replaced_ids {
            let mut stmt = tx.prepare(
                "SELECT line_no, ord, kind, content, timestamp, uuid FROM src.messages
                 WHERE session_id = ?1 ORDER BY line_no, ord",
            )?;
            let msgs: Vec<(i64, i64, String, String, Option<String>, Option<String>)> = stmt
                .query_map(params![sid], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            // 消息 rowid 重插后会变——记录旧→新映射，memories 的
            // source_message_id 必须跟着重映射（终审 blocker）
            for (line_no, ord, kind, content, timestamp, uuid) in &msgs {
                let old_id: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM src.messages WHERE session_id = ?1 AND line_no = ?2 AND ord = ?3",
                        params![sid, line_no, ord],
                        |r| r.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "INSERT INTO messages(session_id, line_no, ord, kind, content, timestamp, uuid)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![sid, line_no, ord, kind, content, timestamp, uuid],
                )?;
                if let Some(oid) = old_id {
                    msg_id_map.insert(oid, tx.last_insert_rowid());
                }
            }
            messages_added += msgs.len() as u64;
            tx.execute(
                "INSERT OR IGNORE INTO vault_lines(session_id, line_no, hash)
                 SELECT session_id, line_no, hash FROM src.vault_lines WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO session_uuids(session_id, line_no, uuid)
                 SELECT session_id, line_no, uuid FROM src.session_uuids WHERE session_id = ?1",
                params![sid],
            )?;
        }

        // small tables: PK / unique-constraint merges
        tx.execute(
            "INSERT OR IGNORE INTO session_links(child_session_id, parent_session_id, link_type, via_uuid, created_at)
             SELECT child_session_id, parent_session_id, link_type, via_uuid, created_at FROM src.session_links",
            [],
        )?;
        // session_artifacts / memories：project_id 必须过 project_map 重映射——
        // src 的项目 id 在 target 里可能是另一个项目，直插会静默挂错（FK 只在
        // id 恰好不存在时才会报错，撞上存在的 id 更糟：无声错挂）。
        let mut artifacts_added = 0u64;
        {
            let mut stmt = tx.prepare(
                "SELECT session_id, project_id, path, tool, created_at FROM src.session_artifacts",
            )?;
            let rows: Vec<(String, Option<i64>, String, Option<String>, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (sid, pid, path, tool, created) in rows {
                let new_pid = pid.and_then(|p| project_map.get(&p).copied());
                artifacts_added += tx.execute(
                    "INSERT OR IGNORE INTO session_artifacts(session_id, project_id, path, tool, created_at)
                     VALUES (?1,?2,?3,?4,?5)",
                    params![sid, new_pid, path, tool, created],
                )? as u64;
            }
        }
        let mut memories_added = 0u64;
        {
            let mut stmt = tx.prepare(
                "SELECT id, project_id, scope, type, content, status, source_session_id,
                        source_message_id, superseded_by, created_at, updated_at FROM src.memories",
            )?;
            let rows: Vec<(String, Option<i64>, String, String, String, String, Option<String>, Option<i64>, Option<String>, String, String)> = stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (id, pid, scope, typ, content, status, ssid, smid, sup, created, updated) in rows {
                let new_pid = pid.and_then(|p| project_map.get(&p).copied());
                // source_message_id 重映射（终审 blocker）：bundle 侧旧消息 id →
                // 本库新 id；映射缺失则置 NULL——错挂比缺挂严重
                let new_smid = smid.and_then(|m| msg_id_map.get(&m).copied());
                memories_added += tx.execute(
                    "INSERT OR IGNORE INTO memories(id, project_id, scope, type, content, status,
                        source_session_id, source_message_id, superseded_by, created_at, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    params![id, new_pid, scope, typ, content, status, ssid, new_smid, sup, created, updated],
                )? as u64;
            }
        }
        // purged_sources 墓碑随迁（v6）：合并不带墓碑的话，目标库 import 会把
        // 已清除的会话复活（codex 八审）。v3-v5 旧 bundle 没这张表——探测后跳过。
        {
            let mut st = tx.prepare("PRAGMA src.table_info(purged_sources)")?;
            let has: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if !has.is_empty() {
                tx.execute(
                    "INSERT OR IGNORE INTO purged_sources(path, session_id, purged_at)
                     SELECT path, session_id, purged_at FROM src.purged_sources",
                    [],
                )?;
            }
        }
        // source_files: offset 取 max（DESIGN-0.3 §5.1）。
        // v7 的 saw_response_item 一并合并（取 max，标志置 1 后不回退）：不带的话
        // 合并来的 codex 文件游标已就位而标志为 0，后续纯 event_msg 增量会双录
        // （codex 评审 blocker）。v6 及更早的 bundle 没这列——探测后按 0 并入。
        // （INSERT...SELECT...ON CONFLICT 的 SELECT 必须带 WHERE，否则 SQLite 报语法错）
        let has_saw = {
            let cols: Vec<String> = {
                let mut st = tx.prepare("PRAGMA src.table_info(source_files)")?;
                let rows = st.query_map([], |r| r.get::<_, String>(1))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            cols.iter().any(|c| c == "saw_response_item")
        };
        if has_saw {
            tx.execute(
                "INSERT INTO source_files(path, agent, imported_bytes, line_count, cursor_text,
                                          saw_response_item, updated_at)
                 SELECT path, agent, imported_bytes, line_count, cursor_text, saw_response_item, updated_at
                 FROM src.source_files WHERE 1=1
                 ON CONFLICT(agent, path) DO UPDATE SET
                   imported_bytes = MAX(source_files.imported_bytes, excluded.imported_bytes),
                   line_count = MAX(source_files.line_count, excluded.line_count),
                   cursor_text = COALESCE(excluded.cursor_text, source_files.cursor_text),
                   saw_response_item = MAX(source_files.saw_response_item, excluded.saw_response_item),
                   updated_at = MAX(source_files.updated_at, excluded.updated_at)",
                [],
            )?;
        } else {
            tx.execute(
                "INSERT INTO source_files(path, agent, imported_bytes, line_count, cursor_text, updated_at)
                 SELECT path, agent, imported_bytes, line_count, cursor_text, updated_at FROM src.source_files
                 WHERE 1=1
                 ON CONFLICT(agent, path) DO UPDATE SET
                   imported_bytes = MAX(source_files.imported_bytes, excluded.imported_bytes),
                   line_count = MAX(source_files.line_count, excluded.line_count),
                   cursor_text = COALESCE(excluded.cursor_text, source_files.cursor_text),
                   updated_at = MAX(source_files.updated_at, excluded.updated_at)",
                [],
            )?;
        }
        // handoffs: project_id 重映射后追加（id 新分配）。同一 bundle 重复 --merge
        // 是常见误操作，按 (project_id, session_id, title, created_at) 查重跳过——
        // latest_handoff 按 id DESC 取，重复行会翻倍进卷宗
        let mut handoffs_added = 0u64;
        let mut fts_stale: Vec<i64> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT project_id, session_id, title, done, state, decisions, files_changed,
                        open_issues, next_steps, created_at FROM src.handoffs",
            )?;
            let rows: Vec<(Option<i64>, Option<String>, String, String, String, String, String, String, String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (pid, sid, title, done, state, decisions, files, issues, next, created) in rows {
                let new_pid = pid.and_then(|p| project_map.get(&p).copied());
                let Some(new_pid) = new_pid else { continue };
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM handoffs WHERE project_id = ?1 AND session_id IS ?2
                           AND title = ?3 AND created_at = ?4",
                        params![new_pid, sid, title, created],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_some() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO handoffs(project_id, session_id, title, done, state, decisions,
                                          files_changed, open_issues, next_steps, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![new_pid, sid, title, done, state, decisions, files, issues, next, created],
                )?;
                handoffs_added += 1;
            }
        }
        // memory_files: 按 (agent, path) 合并；memory_revisions 按 (file_id, hash) 查重
        let mut memory_revisions_added = 0u64;
        {
            let mut file_map: HashMap<i64, i64> = HashMap::new();
            let mut stmt = tx.prepare(
                "SELECT id, agent, scope, path, current_hash, updated_at FROM src.memory_files",
            )?;
            let rows: Vec<(i64, String, String, String, String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (old_id, agent, scope, path, hash, updated) in rows {
                let existing: Option<(i64, String)> = tx
                    .query_row(
                        "SELECT id, updated_at FROM memory_files WHERE agent = ?1 AND path = ?2",
                        params![agent, path],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let new_id = match existing {
                    Some((id, cur_updated)) => {
                        if updated > cur_updated {
                            tx.execute(
                                "UPDATE memory_files SET current_hash = ?1, updated_at = ?2 WHERE id = ?3",
                                params![hash, updated, id],
                            )?;
                            fts_stale.push(id);
                        }
                        id
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO memory_files(agent, scope, path, current_hash, updated_at) VALUES (?1,?2,?3,?4,?5)",
                            params![agent, scope, path, hash, updated],
                        )?;
                        tx.last_insert_rowid()
                    }
                };
                file_map.insert(old_id, new_id);
            }
            let mut stmt = tx.prepare("SELECT file_id, hash, size, captured_at FROM src.memory_revisions")?;
            let rows: Vec<(i64, String, i64, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (old_fid, hash, size, captured) in rows {
                let Some(new_fid) = file_map.get(&old_fid).copied() else { continue };
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM memory_revisions WHERE file_id = ?1 AND hash = ?2",
                        params![new_fid, hash],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_none() {
                    tx.execute(
                        "INSERT INTO memory_revisions(file_id, hash, size, captured_at) VALUES (?1,?2,?3,?4)",
                        params![new_fid, hash, size, captured],
                    )?;
                    memory_revisions_added += 1;
                }
            }
        }

        // 被替换会话上既有 memory 的指针按坐标重指新行（终审二轮：不修就是
        // 悬挂 rowid——指向已被 delete_session_data 删掉的旧行）
        for (mem_id, _old_id, sid, line_no, ord) in std::mem::take(&mut replaced_pointer_coords) {
            let new_id: Option<i64> = tx
                .query_row(
                    "SELECT id FROM messages WHERE session_id = ?1 AND line_no = ?2 AND ord = ?3",
                    rusqlite::params![sid, line_no, ord],
                    |r| r.get(0),
                )
                .optional()?;
            // 按 memory id 更新（终审三轮）：旧消息 rowid 会被重插复用，
            // 按它选择会命中刚并入/刚重指的其他 memory，造成连环错挂
            match new_id {
                Some(nid) => {
                    tx.execute(
                        "UPDATE memories SET source_message_id = ?1 WHERE id = ?2",
                        rusqlite::params![nid, mem_id],
                    )?;
                }
                None => {
                    // 替换后的会话里没有这个坐标了（bundle 侧更旧）——指针置
                    // NULL，比悬挂好
                    tx.execute(
                        "UPDATE memories SET source_message_id = NULL WHERE id = ?1",
                        rusqlite::params![mem_id],
                    )?;
                }
            }
        }

        // 墓碑让位于显式迁移（终审 blocker）：bundle 带来的会话若在目标库有
        // 墓碑（曾 purge），保留会让"会话存在但源增量被阻断"的矛盾态——
        // 迁移意图优先，删墓碑放行后续导入。
        tx.execute(
            "DELETE FROM purged_sources WHERE session_id IN (SELECT id FROM src.sessions)",
            [],
        )?;

        tx.commit()?;
        Ok(json!({
            "sessions_added": sessions_added,
            "sessions_replaced": sessions_replaced,
            "sessions_skipped": sessions_skipped,
            "messages_added": messages_added,
            "artifacts_added": artifacts_added,
            "memories_added": memories_added,
            "handoffs_added": handoffs_added,
            "memory_revisions_added": memory_revisions_added,
            "memory_fts_stale": fts_stale,
        }))
    })();
    target.execute("DETACH DATABASE src", [])?;
    r
}

fn insert_session(conn: &Connection, s: &Value, project_map: &HashMap<i64, i64>) -> Result<()> {
    let new_pid = s["project_id"].as_i64().and_then(|p| project_map.get(&p).copied());
    // created_at/updated_at 按原文直插——合并要透明，不重置时间线
    conn.execute(
        "INSERT INTO sessions(id, agent, native_id, project_id, file_path, cwd, git_branch,
                              started_at, ended_at, message_count, first_parent_uuid,
                              compact_leaf_uuid, created_at, updated_at, deleted_at, compact_line_no)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        params![
            s["id"].as_str(), s["agent"].as_str(), s["native_id"].as_str(), new_pid,
            s["file_path"].as_str(), s["cwd"].as_str(), s["git_branch"].as_str(),
            s["started_at"].as_str(), s["ended_at"].as_str(), s["message_count"].as_i64().unwrap_or(0),
            s["first_parent_uuid"].as_str(), s["compact_leaf_uuid"].as_str(),
            s["created_at"].as_str(), s["updated_at"].as_str(), s["deleted_at"].as_str(),
            s["compact_line_no"].as_i64(),
        ],
    )?;
    Ok(())
}
