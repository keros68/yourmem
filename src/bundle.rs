//! Bundle: one-shot migration backup & restore (DESIGN-0.3 §5). 搬家，不是同步。
//!
//! Layout inside the `.tar.gz`:
//!   <stem>/manifest.json        — format/app/schema version, object counts, filter
//!   <stem>/db/<ts>.sqlite       — VACUUM INTO consistency snapshot
//!   <stem>/objects/<shard>/<hash> — CAS objects, verbatim copies
//!   <stem>/config.json          — allow-listed settings (unfiltered v3+ only)
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

pub const FORMAT_VERSION: u32 = 3;
const CONFIG_FORMAT_VERSION: u32 = 1;

fn source_config(home: &Path) -> Result<Value> {
    let path = home.join("config.json");
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: Value =
                serde_json::from_str(&text).context("config.json 无法解析，未创建不完整备份")?;
            anyhow::ensure!(
                value.is_object(),
                "config.json 不是 JSON 对象，未创建不完整备份"
            );
            Ok(value)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}

/// Configuration is exported by allow-list, not by a sensitive-word deny-list:
/// newly introduced fields and nested values stay private until reviewed here.
fn sanitize_config(input: &Value) -> Result<(Value, Vec<String>)> {
    let object = input.as_object().context("config.json 不是 JSON 对象")?;
    let known = [
        "extra_roots",
        "disabled_agents",
        "backup_dir",
        "auto_purge_trash",
        "snapshot_interval_days",
        "snapshot_keep_recent",
        "snapshot_keep_monthly",
        "snapshot_auto_cleanup",
        "index_tool_output",
        "tracked_docs",
        "ai",
    ];
    let mut excluded: Vec<String> = object
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .cloned()
        .collect();
    let mut out = serde_json::Map::new();

    if let Some(value) = object.get("extra_roots") {
        let rows = value.as_array().context("config extra_roots 必须是数组")?;
        let mut clean = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let row = row.as_object().context("config extra_roots 项必须是对象")?;
            let agent = row
                .get("agent")
                .and_then(Value::as_str)
                .context("config extra_roots.agent 必须是字符串")?;
            let path = row
                .get("path")
                .and_then(Value::as_str)
                .context("config extra_roots.path 必须是字符串")?;
            anyhow::ensure!(
                !agent.is_empty() && !path.is_empty(),
                "config extra_roots 不能含空 agent/path"
            );
            excluded.extend(
                row.keys()
                    .filter(|key| !["agent", "path"].contains(&key.as_str()))
                    .map(|key| format!("extra_roots[{index}].{key}")),
            );
            clean.push(json!({"agent": agent, "path": path}));
        }
        out.insert("extra_roots".into(), Value::Array(clean));
    }
    if let Some(value) = object.get("disabled_agents") {
        let rows = value
            .as_array()
            .context("config disabled_agents 必须是数组")?;
        anyhow::ensure!(
            rows.iter().all(Value::is_string),
            "config disabled_agents 必须只含字符串"
        );
        out.insert("disabled_agents".into(), value.clone());
    }
    if let Some(value) = object.get("backup_dir") {
        anyhow::ensure!(value.is_string(), "config backup_dir 必须是字符串");
        out.insert("backup_dir".into(), value.clone());
    }
    for key in [
        "auto_purge_trash",
        "snapshot_auto_cleanup",
        "index_tool_output",
    ] {
        if let Some(value) = object.get(key) {
            anyhow::ensure!(value.is_boolean(), "config {key} 必须是布尔值");
            out.insert(key.into(), value.clone());
        }
    }
    for key in [
        "snapshot_interval_days",
        "snapshot_keep_recent",
        "snapshot_keep_monthly",
    ] {
        if let Some(value) = object.get(key) {
            anyhow::ensure!(value.as_u64().is_some(), "config {key} 必须是非负整数");
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(value) = object.get("tracked_docs") {
        let rows = value.as_array().context("config tracked_docs 必须是数组")?;
        let mut clean = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let row = row
                .as_object()
                .context("config tracked_docs 项必须是对象")?;
            let project = row
                .get("project")
                .and_then(Value::as_str)
                .context("config tracked_docs.project 必须是字符串")?;
            let path = row
                .get("path")
                .and_then(Value::as_str)
                .context("config tracked_docs.path 必须是字符串")?;
            anyhow::ensure!(
                !project.is_empty() && !path.is_empty(),
                "config tracked_docs 不能含空 project/path"
            );
            excluded.extend(
                row.keys()
                    .filter(|key| !["project", "path"].contains(&key.as_str()))
                    .map(|key| format!("tracked_docs[{index}].{key}")),
            );
            clean.push(json!({"project": project, "path": path}));
        }
        out.insert("tracked_docs".into(), Value::Array(clean));
    }
    if let Some(value) = object.get("ai") {
        let ai = value.as_object().context("config ai 必须是对象")?;
        excluded.extend(
            ai.keys()
                .filter(|key| !["base_url", "model", "max_input_chars"].contains(&key.as_str()))
                .map(|key| format!("ai.{key}")),
        );
        let mut clean = serde_json::Map::new();
        if let Some(base_url) = ai.get("base_url") {
            let raw = base_url
                .as_str()
                .context("config ai.base_url 必须是字符串")?;
            let safe = reqwest::Url::parse(raw).ok().is_some_and(|url| {
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            });
            if safe {
                clean.insert("base_url".into(), base_url.clone());
            } else {
                excluded.push("ai.base_url".into());
            }
        }
        if let Some(model) = ai.get("model") {
            anyhow::ensure!(model.is_string(), "config ai.model 必须是字符串");
            clean.insert("model".into(), model.clone());
        }
        if let Some(max) = ai.get("max_input_chars") {
            anyhow::ensure!(
                max.as_u64().is_some(),
                "config ai.max_input_chars 必须是非负整数"
            );
            clean.insert("max_input_chars".into(), max.clone());
        }
        out.insert("ai".into(), Value::Object(clean));
    }
    excluded.sort();
    excluded.dedup();
    Ok((Value::Object(out), excluded))
}

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
    let _lock = crate::ingest::ImportLockTx::acquire(home, std::time::Duration::from_secs(120))?;
    let staging = crate::staging_dir_near(out).context("create staging dir")?;
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bundle".to_string());
    // strip a trailing ".tar" so x.tar.gz -> x
    let stem = stem
        .strip_suffix(".tar")
        .map(str::to_string)
        .unwrap_or(stem);
    let root = staging.path().join(&stem);
    std::fs::create_dir_all(root.join("db"))?;
    std::fs::create_dir_all(root.join("objects"))?;

    // 1. Consistent DB snapshot (VACUUM INTO works on a live DB).
    let snap_name = format!(
        "yourmem-{}.sqlite",
        chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f")
    );
    let snap_path = root.join("db").join(&snap_name);
    conn.execute(
        "VACUUM INTO ?1",
        params![snap_path.to_string_lossy().as_ref()],
    )?;

    // 2. Optional filter: prune the snapshot (never the live DB).
    let mut filter_report = Value::Null;
    if !filter.is_empty() {
        filter_report = prune_snapshot(&snap_path, filter)?;
    }
    strip_indexes(&snap_path)?;

    // 3. Copy only the objects still referenced by the (pruned) snapshot.
    let snap = Connection::open_with_flags(&snap_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let keep = referenced_hashes(&snap)?;
    let schema_version: i32 = snap.pragma_query_value(None, "user_version", |r| r.get(0))?;
    drop(snap);

    let mut objects = 0u64;
    let mut bytes = 0u64;
    let mut missing = 0u64;
    let store = vault::Store::open(home)?;
    for hash in &keep {
        let raw = match store.get(hash, true) {
            Ok(raw) => raw,
            Err(_) => {
                missing += 1;
                continue;
            }
        };
        let dst = root
            .join("objects")
            .join(hash.get(..2).unwrap_or(hash))
            .join(hash);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(&dst, &raw)?;
        objects += 1;
        bytes += raw.len() as u64;
    }
    // 保真是"验"出来的：引用的对象缺一个，恢复出的库就有导不出来的会话，
    // 这种 bundle 不该被创建（宁可失败也不打包一个自洽但残缺的包）。
    anyhow::ensure!(
        missing == 0,
        "vault 缺 {missing} 个被引用的对象，bundle 未创建——先跑 backup status 检查对象库"
    );

    let mut excluded_config_fields = Vec::new();
    // 配置有问题只跳过配置，不阻断数据备份
    let mut config_skipped: Option<String> = None;
    if filter.is_empty() {
        match source_config(home).and_then(|raw| sanitize_config(&raw)) {
            Ok((config, excluded)) => {
                excluded_config_fields = excluded;
                std::fs::write(
                    root.join("config.json"),
                    serde_json::to_string_pretty(&config)?,
                )?;
            }
            Err(e) => config_skipped = Some(format!("{e:#}")),
        }
    }
    let config_included = filter.is_empty() && config_skipped.is_none();

    let manifest = json!({
        "format_version": FORMAT_VERSION,
        "app_version": env!("CARGO_PKG_VERSION"),
        "schema_version": schema_version,
        "created_at": crate::now_iso(),
        "db_snapshot": snap_name,
        "objects": objects,
        "objects_bytes": bytes,
        "filter": filter_report,
        "indexes_omitted": true,
        "config": {
            "format_version": CONFIG_FORMAT_VERSION,
            "included": config_included,
            "source_home": if config_included { Value::from(home.to_string_lossy().to_string()) } else { Value::Null },
            "source_user_home": if config_included { Value::from(crate::home_dir().to_string_lossy().to_string()) } else { Value::Null },
            "excluded_fields": excluded_config_fields,
            "skipped_reason": config_skipped,
        },
    });
    std::fs::write(
        root.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    // 4. Pack tar.gz.
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Publish only a complete archive; failed compression must not truncate an older backup.
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    let file = pending.as_file_mut();
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(&stem, &root)?;
    tar.into_inner()?.finish()?;
    pending.as_file().sync_all()?;
    pending.persist(out).map_err(|e| e.error)?;
    Ok(manifest)
}

/// Delete everything in the snapshot DB that doesn't match the filter.
/// 删除顺序遵守外键：delete_session_data 连带子表（messages/vault_lines/
/// session_uuids）→ 依赖会话的小表 → handoffs/memories/artifacts → projects；
/// memory_revisions → memory_files。
fn prune_snapshot(snap_path: &Path, filter: &BundleFilter) -> Result<Value> {
    let conn = Connection::open(snap_path)?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;")?;

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
                // 每个会话有自己的采集游标（文件型按文件路径，opencode/hermes 按
                // `<agent>://<会话 id>`，都记在 sessions.file_path）：一并裁掉，否则
                // 恢复端 import 会跳过这些内容，被裁会话永远采不回且无提示。
                conn.execute(
                    "DELETE FROM source_files WHERE agent = ?1 AND path = ?2",
                    params![agent, fpath],
                )?;
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
            &format!(
                "DELETE FROM memories WHERE project_id IS NULL OR project_id NOT IN ({keep_proj})"
            ),
            params![proj],
        )?;
        conn.execute(
            &format!("DELETE FROM memory_fts WHERE rowid IN (SELECT id FROM memory_files
                WHERE scope NOT IN (SELECT 'project:' || path FROM projects WHERE id IN ({keep_proj})))"),
            params![proj],
        )?;
        conn.execute(
            &format!("DELETE FROM memory_revisions WHERE file_id IN (SELECT id FROM memory_files
                WHERE scope NOT IN (SELECT 'project:' || path FROM projects WHERE id IN ({keep_proj})))"),
            params![proj],
        )?;
        conn.execute(
            &format!(
                "DELETE FROM memory_files WHERE scope NOT IN
                (SELECT 'project:' || path FROM projects WHERE id IN ({keep_proj}))"
            ),
            params![proj],
        )?;
        conn.execute(
            &format!("DELETE FROM session_artifacts WHERE project_id IS NOT NULL AND project_id NOT IN ({keep_proj})"),
            params![proj],
        )?;
        conn.execute(
            &format!("DELETE FROM cross_checks WHERE project_id NOT IN ({keep_proj})"),
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
    // 被裁内容不能残留在 FTS 段与空闲页里：随后的 strip_indexes 清空全部
    // 全文索引并以 secure_delete 整库 VACUUM，这里不必重建。
    if filter.project.is_some() {
        conn.execute("DELETE FROM usage_log", [])?;
    }
    if keep_sessions.is_some() {
        conn.execute_batch(
            "DELETE FROM artifact_occurrences WHERE session_id NOT IN (SELECT id FROM sessions);
             DELETE FROM artifact_events WHERE session_id NOT IN (SELECT id FROM sessions);
             DELETE FROM task_steps WHERE session_id NOT IN (SELECT id FROM sessions);
             DELETE FROM task_verifications WHERE session_id IS NOT NULL AND session_id NOT IN (SELECT id FROM sessions);
             DELETE FROM task_sessions WHERE session_id NOT IN (SELECT id FROM sessions);
             DELETE FROM memory_events WHERE session_id IS NOT NULL AND session_id NOT IN (SELECT id FROM sessions);
             DELETE FROM task_claims WHERE claimed_session_id IS NOT NULL AND claimed_session_id NOT IN (SELECT id FROM sessions);",
        )?;
    }
    if let Some(proj) = &filter.project {
        conn.execute(
            "CREATE TEMP TABLE bundle_keep_projects(id INTEGER PRIMARY KEY)",
            [],
        )?;
        conn.execute("INSERT INTO temp.bundle_keep_projects SELECT id FROM projects WHERE name=?1 OR path LIKE '%'||?1||'%'", params![proj])?;
        conn.execute_batch(
            "DELETE FROM task_state_proposals WHERE task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM task_links WHERE source_task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects))
                OR target_task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM task_sessions WHERE task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM task_claims WHERE task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM task_verifications WHERE task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM task_steps WHERE task_id IS NOT NULL AND task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM task_events WHERE task_id NOT IN (SELECT id FROM tasks WHERE project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM tasks WHERE project_id NOT IN (SELECT id FROM temp.bundle_keep_projects);
             DELETE FROM artifact_occurrences WHERE artifact_id NOT IN (SELECT id FROM artifact_events WHERE project_id IS NULL OR project_id IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM artifact_events WHERE project_id IS NOT NULL AND project_id NOT IN (SELECT id FROM temp.bundle_keep_projects);
             DELETE FROM memory_events WHERE memory_id NOT IN (SELECT id FROM memories);
             DELETE FROM memory_labels WHERE label_id IN (SELECT id FROM labels WHERE project_id IS NOT NULL AND project_id NOT IN (SELECT id FROM temp.bundle_keep_projects));
             DELETE FROM labels WHERE project_id IS NOT NULL AND project_id NOT IN (SELECT id FROM temp.bundle_keep_projects);
             DELETE FROM project_record_pins WHERE project_id NOT IN (SELECT id FROM temp.bundle_keep_projects);
             DELETE FROM memory_labels;
             DELETE FROM labels;
             DELETE FROM work_meta WHERE key NOT LIKE 'legacy_task:%';
             DELETE FROM work_meta WHERE key LIKE 'legacy_task:%' AND substr(key,13) NOT IN (SELECT id FROM memories);
             DELETE FROM merge_conflicts;"
        )?;
        conn.execute("DROP TABLE temp.bundle_keep_projects", [])?;
    }
    conn.execute("DELETE FROM change_log", [])?;
    Ok(
        json!({ "agent": filter.agent, "project": filter.project, "sessions_pruned": pruned,
        "includes_global_memory": filter.project.is_none() }),
    )
}

pub(crate) fn strip_indexes(path: &Path) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA secure_delete=ON;
        INSERT INTO messages_fts(messages_fts) VALUES('delete-all');
        INSERT INTO messages_tools_fts(messages_tools_fts) VALUES('delete-all');
        INSERT INTO memories_fts(memories_fts) VALUES('delete-all');
        DELETE FROM memory_fts;
        VACUUM;",
    )?;
    Ok(())
}

fn rebuild_indexes(path: &Path, object_home: &Path) -> Result<()> {
    let mut conn = Connection::open(path)?;
    let full = db::tool_index_enabled(&conn)?;
    let tx = conn.transaction()?;
    tx.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('delete-all');
        INSERT INTO messages_tools_fts(messages_tools_fts) VALUES('delete-all');
        INSERT INTO messages_fts(rowid,content) SELECT id,content FROM messages WHERE kind <> 'tool_result';
        INSERT INTO memories_fts(memories_fts) VALUES('rebuild');
        DELETE FROM memory_fts;")?;
    if full {
        tx.execute("INSERT INTO messages_tools_fts(rowid,content) SELECT id,content FROM messages WHERE kind='tool_result'", [])?;
    }
    let rows = tx
        .prepare("SELECT id,current_hash FROM memory_files")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, hash) in rows {
        let bytes = vault::read_object(object_home, &hash, true)?;
        db::set_memory_fts(&tx, id, std::str::from_utf8(&bytes)?)?;
    }
    tx.commit()?;
    Ok(())
}

/// Hashes still referenced by a (possibly pruned) snapshot.
pub(crate) fn referenced_hashes(conn: &Connection) -> Result<HashSet<String>> {
    let mut keep = HashSet::new();
    for sql in [
        "SELECT DISTINCT hash FROM vault_lines",
        "SELECT DISTINCT hash FROM memory_revisions",
    ] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        keep.extend(rows);
    }
    Ok(keep)
}

struct BundleLayout {
    root: PathBuf, // extracted <stem>/ dir
    manifest: Value,
}

fn extract(bundle: &Path) -> Result<(tempfile::TempDir, BundleLayout)> {
    let tmp = crate::staging_dir_near(bundle).context("extract staging")?;
    let file = std::fs::File::open(bundle).with_context(|| format!("open {}", bundle.display()))?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(dec);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        anyhow::ensure!(
            kind.is_file() || kind.is_dir(),
            "bundle 含不支持的链接或特殊文件"
        );
        anyhow::ensure!(entry.unpack_in(tmp.path())?, "bundle 文件路径越界");
    }
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
        matches!(manifest["format_version"].as_u64(), Some(1 | 2 | 3)),
        "unsupported bundle format_version: {}",
        manifest["format_version"]
    );
    let snapshot = manifest["db_snapshot"]
        .as_str()
        .context("bundle 缺少数据库文件名")?;
    let mut components = Path::new(snapshot).components();
    anyhow::ensure!(
        matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none()
            && !snapshot.contains(['/', '\\']),
        "非法数据库文件名"
    );
    let root = manifest_path.parent().unwrap().to_path_buf();
    Ok((tmp, BundleLayout { root, manifest }))
}

/// Verify a bundle: re-hash every object against its content address and
/// Reconcile counts, database integrity, schema and referenced object coverage.
pub fn verify(bundle: &Path) -> Result<Value> {
    let (_tmp, layout) = extract(bundle)?;
    verify_layout(&layout)
}

#[derive(Clone, Copy)]
enum ConfigPathKind {
    Directory,
    Backup,
}

fn expand_config_path(raw: &str, source_home: &Path) -> PathBuf {
    if raw == "~" {
        return source_home.to_path_buf();
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return source_home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn plan_config_path(
    raw: &str,
    field: &str,
    kind: ConfigPathKind,
    source_user_home: &Path,
    source_data_home: &Path,
    target_user_home: &Path,
    target_data_home: &Path,
    mappings: &HashMap<String, String>,
) -> (String, Value) {
    let original = expand_config_path(raw, source_user_home);
    if let Some(mapped) = mappings.get(raw) {
        let candidate = PathBuf::from(mapped);
        let valid = candidate.is_absolute()
            && match kind {
                ConfigPathKind::Directory => candidate.is_dir(),
                ConfigPathKind::Backup => !candidate.exists() || candidate.is_dir(),
            };
        return (
            mapped.clone(),
            json!({
                "field": field, "source": raw, "candidate": mapped,
                "status": if valid { "remapped" } else { "invalid" },
                "path_remapping_required": !valid,
            }),
        );
    }
    for (source, target) in [
        (source_data_home, target_data_home),
        (source_user_home, target_user_home),
    ] {
        if source != target && original.is_absolute() && original.starts_with(source) {
            let relative = original
                .strip_prefix(source)
                .unwrap_or_else(|_| Path::new(""));
            return (
                raw.to_string(),
                json!({
                    "field":field,"source":raw,"candidate":target.join(relative),
                    "status":"remap_available","path_remapping_required":true,
                }),
            );
        }
    }
    // 备份目录在新机器上可以尚未创建，与显式映射分支同一判据
    let valid = match kind {
        ConfigPathKind::Directory => original.is_dir(),
        ConfigPathKind::Backup => {
            original.is_absolute() && (!original.exists() || original.is_dir())
        }
    };
    (
        raw.to_string(),
        json!({
            "field": field, "source": raw, "candidate": Value::Null,
            "status": if valid { "ok" } else { "missing" },
            "path_remapping_required": !valid,
        }),
    )
}

fn read_target_config(target_home: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(target_home.join("config.json")) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text).context("目标 config.json 无法解析")?;
            anyhow::ensure!(value.is_object(), "目标 config.json 不是 JSON 对象");
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn config_restore_plan(
    layout: &BundleLayout,
    target_home: &Path,
    mappings: &HashMap<String, String>,
) -> Result<(Value, Option<Value>)> {
    if layout.manifest["format_version"].as_u64().unwrap_or(0) < 3
        || layout.manifest["config"]["included"] != true
    {
        return Ok((
            json!({
                "included": false, "status": "not_included",
                "skipped_reason": layout.manifest["config"]["skipped_reason"].clone(),
                "path_remapping_required": false, "config_restore_required": false,
            }),
            None,
        ));
    }
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(layout.root.join("config.json"))?)?;
    let source_home = PathBuf::from(
        layout.manifest["config"]["source_home"]
            .as_str()
            .unwrap_or_default(),
    );
    let source_user_home = layout.manifest["config"]["source_user_home"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home_dir());
    let target_user_home = crate::home_dir();
    let mut paths = Vec::new();
    let mut suggested_mappings = serde_json::Map::new();
    let mut plan_one = |raw: &str, field: &str, kind: ConfigPathKind| {
        let (value, report) = plan_config_path(
            raw,
            field,
            kind,
            &source_user_home,
            &source_home,
            &target_user_home,
            target_home,
            mappings,
        );
        if report["status"] == "remap_available" {
            suggested_mappings.insert(raw.to_string(), report["candidate"].clone());
        }
        paths.push(report);
        value
    };
    if let Some(roots) = config.get_mut("extra_roots").and_then(Value::as_array_mut) {
        for (index, root) in roots.iter_mut().enumerate() {
            let raw = root["path"].as_str().unwrap_or_default().to_string();
            root["path"] = Value::String(plan_one(
                &raw,
                &format!("extra_roots[{index}].path"),
                ConfigPathKind::Directory,
            ));
        }
    }
    if let Some(docs) = config.get_mut("tracked_docs").and_then(Value::as_array_mut) {
        for (index, doc) in docs.iter_mut().enumerate() {
            let raw = doc["project"].as_str().unwrap_or_default().to_string();
            doc["project"] = Value::String(plan_one(
                &raw,
                &format!("tracked_docs[{index}].project"),
                ConfigPathKind::Directory,
            ));
        }
    }
    if let Some(raw) = config["backup_dir"].as_str().map(str::to_string) {
        config["backup_dir"] = Value::String(plan_one(&raw, "backup_dir", ConfigPathKind::Backup));
    }

    let mut errors = Vec::new();
    let existing = match read_target_config(target_home) {
        Ok(value) => value,
        Err(e) => {
            errors.push(e.to_string());
            None
        }
    };
    let conflicts = existing
        .as_ref()
        .and_then(Value::as_object)
        .map(|local| {
            config
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, value)| local.get(*key).is_some_and(|current| current != *value))
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let path_remapping_required = paths
        .iter()
        .any(|path| path["path_remapping_required"] == true);
    let invalid_paths: Vec<Value> = paths
        .iter()
        .filter(|path| path["status"] == "invalid")
        .cloned()
        .collect();
    let config_restore_required = !conflicts.is_empty() || !errors.is_empty();
    let status = if !errors.is_empty() {
        "error"
    } else if !invalid_paths.is_empty() {
        "invalid_path"
    } else if path_remapping_required {
        "path_remapping_required"
    } else if !conflicts.is_empty() {
        "config_conflict"
    } else {
        "ready"
    };
    Ok((
        json!({
            "included": true, "status": status, "paths": paths,
            "suggested_mappings": suggested_mappings, "invalid_paths": invalid_paths,
            "path_remapping_required": path_remapping_required,
            "config_restore_required": config_restore_required,
            "conflicts": conflicts, "errors": errors,
        }),
        Some(config),
    ))
}

fn apply_config_plan(
    target_home: &Path,
    config: Option<Value>,
    mut report: Value,
    overwrite_existing: bool,
) -> Result<Value> {
    let Some(config) = config else {
        return Ok(report);
    };
    if report["status"] == "not_included"
        || !report["errors"].as_array().is_some_and(Vec::is_empty)
        || report["path_remapping_required"] == true
    {
        return Ok(report);
    }
    if !report["conflicts"].as_array().is_some_and(Vec::is_empty) && !overwrite_existing {
        return Ok(report);
    }
    let mut changed = false;
    let mut conflicted = false;
    let mut held_back: Vec<String> = Vec::new();
    let update = crate::ingest::update_config(target_home, |current| {
        let local = current
            .as_object()
            .context("目标 config.json 不是 JSON 对象")?;
        conflicted = config
            .as_object()
            .into_iter()
            .flatten()
            .any(|(key, value)| local.get(key).is_some_and(|existing| existing != value));
        if conflicted && !overwrite_existing {
            report["status"] = Value::String("config_conflict".into());
            report["config_restore_required"] = Value::Bool(true);
            return Ok(());
        }
        let local = current.as_object_mut().unwrap();
        for (key, value) in config.as_object().unwrap() {
            // 自动清空回收站不随恢复开启：恢复常为找回回收站内容，开启后下次启动就会清理
            if key == "auto_purge_trash"
                && value == &Value::Bool(true)
                && local.get(key) != Some(value)
            {
                held_back.push(key.clone());
                continue;
            }
            if local.get(key) != Some(value) {
                local.insert(key.clone(), value.clone());
                changed = true;
            }
        }
        Ok(())
    });
    if let Err(e) = update {
        report["status"] = Value::String("error".into());
        report["config_restore_required"] = Value::Bool(true);
        report["errors"]
            .as_array_mut()
            .unwrap()
            .push(Value::String(e.to_string()));
        return Ok(report);
    }
    if conflicted && !overwrite_existing {
        return Ok(report);
    }
    report["status"] = Value::String(if changed { "restored" } else { "unchanged" }.into());
    report["config_restore_required"] = Value::Bool(false);
    report["conflicts"] = json!([]);
    report["changed"] = Value::Bool(changed);
    report["held_back"] = json!(held_back);
    Ok(report)
}

/// Apply bundled settings after reviewing `restore_plan`. `path_mappings` keys
/// are the exact source paths from the preview. Existing conflicting settings
/// are preserved unless `overwrite_existing` is explicitly true; unrelated
/// local fields are always preserved.
pub fn restore_config(
    bundle: &Path,
    target_home: &Path,
    path_mappings: &HashMap<String, String>,
    overwrite_existing: bool,
) -> Result<Value> {
    let (_tmp, layout) = extract(bundle)?;
    let report = verify_layout(&layout)?;
    anyhow::ensure!(report["ok"] == true, "bundle 校验失败");
    anyhow::ensure!(
        layout.manifest["format_version"].as_u64().unwrap_or(0) >= 3
            && layout.manifest["config"]["included"] == true,
        "bundle 不含配置"
    );
    anyhow::ensure!(
        target_home.join("yourmem.db").is_file(),
        "请先恢复 bundle 数据库，再应用配置"
    );
    let (plan, config) = config_restore_plan(&layout, target_home, path_mappings)?;
    apply_config_plan(target_home, config, plan, overwrite_existing)
}

/// Read-only restore preview. Counts use the same message-count conflict rule as merge.
pub fn restore_plan(bundle: &Path, target_home: &Path) -> Result<Value> {
    let (_tmp, layout) = extract(bundle)?;
    let mut report = verify_layout(&layout)?;
    if report["ok"] != true {
        return Ok(report);
    }
    let schema = report["actual_schema_version"].as_i64().unwrap_or(i64::MAX);
    if schema > i64::from(db::SCHEMA_VERSION) {
        report["ok"] = json!(false);
        report["reason"] = json!("备份来自更新版本，请先升级 yourmem");
        return Ok(report);
    }
    let source = Connection::open_with_flags(
        layout
            .root
            .join("db")
            .join(layout.manifest["db_snapshot"].as_str().unwrap()),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let target_db = target_home.join("yourmem.db");
    let target = if target_db.is_file() {
        Some(Connection::open_with_flags(
            target_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    } else {
        None
    };
    let mut added = 0u64;
    let mut replaced = 0u64;
    let mut skipped = 0u64;
    let mut stmt = source.prepare("SELECT id, message_count FROM sessions")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (sid, count) = row?;
        let existing: Option<i64> = match &target {
            Some(conn) => conn
                .query_row(
                    "SELECT message_count FROM sessions WHERE id=?1",
                    [&sid],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        match existing {
            None => added += 1,
            Some(n) if n < count => replaced += 1,
            Some(_) => skipped += 1,
        }
    }
    report["merge_plan"] = json!({"home": target_home, "sessions_added": added,
        "sessions_replaced": replaced, "sessions_skipped": skipped});
    let work_plan = work_restore_plan(&source, target.as_ref())?;
    report["merge_plan"]["task_forks"] = work_plan["task_forks"].clone();
    report["merge_plan"]["task_alias_remaps"] = work_plan["task_alias_remaps"].clone();
    report["merge_plan"]["relation_conflicts"] = work_plan["relation_conflicts"].clone();
    report["work_plan"] = work_plan;
    let (config_plan, _) = config_restore_plan(&layout, target_home, &HashMap::new())?;
    report["path_remapping_required"] = config_plan["path_remapping_required"].clone();
    report["config_restore_required"] = config_plan["config_restore_required"].clone();
    report["config"] = config_plan;
    Ok(report)
}

fn verify_layout(layout: &BundleLayout) -> Result<Value> {
    let objects_dir = layout.root.join("objects");
    let mut present = HashSet::new();
    let mut objects = 0u64;
    let mut bytes = 0u64;
    let mut corrupted: Vec<String> = Vec::new();
    if objects_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&objects_dir) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let hash = entry.file_name().to_string_lossy().to_string();
            let data = std::fs::read(entry.path())?;
            objects += 1;
            bytes += data.len() as u64;
            let valid = vault::is_hash(&hash);
            if valid && entry.path() == objects_dir.join(&hash[..2]).join(&hash) {
                present.insert(hash.clone());
            } else {
                corrupted.push(hash.clone());
            }
            if vault::hash_bytes(&data) != hash {
                corrupted.push(hash);
            }
        }
    }
    let db_file = layout
        .root
        .join("db")
        .join(layout.manifest["db_snapshot"].as_str().unwrap_or_default());
    let mut db_opens = false;
    let mut actual_schema = None;
    let mut database_errors = Vec::new();
    let mut missing = Vec::new();
    let inspection = (|| -> Result<()> {
        let conn = Connection::open_with_flags(&db_file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db_opens = true;
        validate_source_tables(&conn, "main")?;
        actual_schema =
            Some(conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?);
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let row = row?;
            if row != "ok" {
                database_errors.push(row);
            }
        }
        let foreign_key_errors = conn
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_some();
        if foreign_key_errors {
            database_errors.push("数据库存在外键错误".into());
        }
        for hash in referenced_hashes(&conn)? {
            if !present.contains(&hash) {
                missing.push(hash);
            }
        }
        let stale: i64 = conn.query_row("SELECT COUNT(*) FROM memory_files f
            WHERE NOT EXISTS (SELECT 1 FROM memory_revisions r WHERE r.file_id=f.id AND r.hash=f.current_hash)", [], |r| r.get(0))?;
        if stale > 0 {
            database_errors.push(format!("{stale} 个记忆文件缺少当前修订"));
        }
        Ok(())
    })();
    if let Err(e) = inspection {
        database_errors.push(e.to_string());
    }
    missing.sort();
    let expected = &layout.manifest;
    let mut config_errors = Vec::new();
    if expected["format_version"].as_u64() == Some(3) {
        let config_path = layout.root.join("config.json");
        let inspection = (|| -> Result<()> {
            if expected["config"]["included"] == true {
                anyhow::ensure!(
                    expected["config"]["format_version"] == CONFIG_FORMAT_VERSION,
                    "不支持的 bundle 配置格式"
                );
                anyhow::ensure!(
                    expected["config"]["source_home"].is_string(),
                    "bundle 配置缺少 source_home"
                );
                let raw: Value = serde_json::from_slice(&std::fs::read(config_path)?)?;
                let (clean, _) = sanitize_config(&raw)?;
                anyhow::ensure!(clean == raw, "bundle 配置含未允许或敏感字段");
            } else {
                anyhow::ensure!(!config_path.exists(), "过滤 bundle 不得包含个人配置");
            }
            Ok(())
        })();
        if let Err(e) = inspection {
            config_errors.push(e.to_string());
        }
    }
    let ok = corrupted.is_empty()
        && objects == expected["objects"].as_u64().unwrap_or(u64::MAX)
        && bytes == expected["objects_bytes"].as_u64().unwrap_or(u64::MAX)
        && db_opens
        && database_errors.is_empty()
        && missing.is_empty()
        && config_errors.is_empty()
        && actual_schema == expected["schema_version"].as_i64();
    Ok(json!({
        "ok": ok,
        "objects": objects,
        "objects_bytes": bytes,
        "corrupted": corrupted,
        "db_snapshot_opens": db_opens,
        "actual_schema_version": actual_schema,
        "database_errors": database_errors,
        "config_errors": config_errors,
        "missing_referenced_objects": missing,
        "manifest": layout.manifest,
    }))
}

/// Restore a bundle into `target_home`. Default: fresh home (换电脑场景);
/// `--merge`: merge into an existing library.
pub fn restore(bundle: &Path, target_home: &Path, merge: bool) -> Result<Value> {
    let (_tmp, layout) = extract(bundle)?;
    let report = verify_layout(&layout)?;
    anyhow::ensure!(
        report["ok"] == true,
        "bundle 校验失败：{}",
        serde_json::to_string_pretty(&report)?
    );
    // 合并只信自己库的 schema（AGENTS.md）：更新版本产出的 bundle 直接拒绝，
    // 不让列不匹配退化成半路崩出的 SQL 错误。
    let bundle_schema = report["manifest"]["schema_version"]
        .as_i64()
        .unwrap_or(i64::MAX);
    anyhow::ensure!(
        bundle_schema <= i64::from(db::SCHEMA_VERSION),
        "bundle schema_version {bundle_schema} 比本程序（{}）新——请先升级 yourmem",
        db::SCHEMA_VERSION
    );
    let src_db_path = layout
        .root
        .join("db")
        .join(layout.manifest["db_snapshot"].as_str().unwrap());
    let indexes_omitted = layout.manifest["indexes_omitted"] == true;
    if indexes_omitted {
        anyhow::ensure!(
            layout.manifest["format_version"]
                .as_u64()
                .is_some_and(|version| version >= 2),
            "索引省略标记需要备份格式 v2+"
        );
    }
    std::fs::create_dir_all(target_home)?;
    let _lock =
        crate::ingest::ImportLockTx::acquire(target_home, std::time::Duration::from_secs(120))?;
    let (config_plan, config_values) = config_restore_plan(&layout, target_home, &HashMap::new())?;

    if !merge {
        let target_db = target_home.join("yourmem.db");
        anyhow::ensure!(
            !target_db.exists(),
            "{} 已存在——恢复到全新目录，或加 --merge 合并",
            target_db.display()
        );
        // 合并只读 bundle 的数据表、索引在目标库里维护；全新恢复才需要补建
        if indexes_omitted {
            rebuild_indexes(&src_db_path, &layout.root)?;
        }
        std::fs::create_dir_all(target_home.join("objects"))?;
        let copied = copy_objects(&layout.root, target_home)
            .context("对象恢复失败，数据库尚未写入；排除磁盘或权限问题后可重试")?;
        let mut pending = tempfile::NamedTempFile::new_in(target_home)?;
        std::io::copy(
            &mut std::fs::File::open(&src_db_path)?,
            pending.as_file_mut(),
        )?;
        pending.as_file().sync_all()?;
        pending.persist_noclobber(&target_db).map_err(|e| e.error)?;
        let restored_conn = db::open(target_home)?;
        crate::work::repair_schema(&restored_conn)?;
        let previous_generation = crate::work::database_generation(&restored_conn)?;
        let database_generation = crate::work::regenerate_generation(&restored_conn)?;
        restored_conn.execute("INSERT INTO change_log(entity_table,entity_key,operation,committed_at) VALUES('restore',?1,'reset',?2)",
            params![database_generation,crate::now_iso()])?;
        let config = apply_config_plan(target_home, config_values, config_plan, false)?;
        let mut result = json!({
            "mode": "fresh", "home": target_home, "objects_copied": copied,
            "previous_generation": previous_generation, "database_generation": database_generation,
            "note": "一次性迁移完成；这不是同步，源机器的后续变化不会自动过来。",
        });
        result["path_remapping_required"] = config["path_remapping_required"].clone();
        result["config_restore_required"] = config["config_restore_required"].clone();
        if result["path_remapping_required"] == true || result["config_restore_required"] == true {
            result["note"] = json!(
                "数据库已恢复；配置有待处理的路径或冲突，尚未写入，请查看 config 并显式处理。"
            );
        }
        result["config"] = config;
        return Ok(result);
    }

    anyhow::ensure!(
        target_home.join("yourmem.db").is_file(),
        "--merge 需要目标已有 yourmem 库"
    );
    let conn = db::open(target_home)?;
    let copied = copy_objects(&layout.root, target_home)
        .context("对象恢复失败，尚未合并数据库；排除磁盘或权限问题后可重试")?;
    let merged = merge_db(&conn, &src_db_path, target_home)
        .context("数据库合并失败，事务已回滚；完整对象保留供重试")?;
    let config = apply_config_plan(target_home, config_values, config_plan, false)?;
    let mut result = json!({
        "mode": "merge", "home": target_home, "objects_copied": copied, "merged": merged,
        "memory_fts_backfilled": merged["memory_fts_backfilled"],
        "note": "一次性合并完成；这不是同步，源机器的后续变化不会自动过来。",
    });
    result["path_remapping_required"] = config["path_remapping_required"].clone();
    result["config_restore_required"] = config["config_restore_required"].clone();
    if result["path_remapping_required"] == true || result["config_restore_required"] == true {
        result["note"] =
            json!("数据库已合并；配置有待处理的路径或冲突，尚未写入，请查看 config 并显式处理。");
    }
    result["config"] = config;
    Ok(result)
}

/// Copy complete objects; reuse verified objects and replace damaged copies.
fn copy_objects(bundle_root: &Path, target_home: &Path) -> Result<u64> {
    let objects_dir = bundle_root.join("objects");
    if !objects_dir.is_dir() {
        return Ok(0);
    }
    let store = vault::Store::open(target_home)?;
    let mut batch: Vec<Vec<u8>> = Vec::new();
    let mut copied = 0u64;
    let flush = |batch: &mut Vec<Vec<u8>>| -> Result<()> {
        store.put_many(batch.iter().map(Vec::as_slice))?;
        batch.clear();
        Ok(())
    };
    for entry in walkdir::WalkDir::new(&objects_dir) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let hash = entry.file_name().to_string_lossy().to_string();
        if store.contains(&hash)? {
            if store.get(&hash, true).is_ok() {
                continue;
            }
            store.remove(&hash)?;
        }
        let raw = std::fs::read(entry.path())?;
        anyhow::ensure!(
            vault::hash_bytes(&raw) == hash,
            "bundle object hash mismatch: {hash}"
        );
        batch.push(raw);
        copied += 1;
        if batch.len() == 1000 {
            flush(&mut batch)?;
        }
    }
    flush(&mut batch)?;
    Ok(copied)
}

/// 给缺 memory_fts 行的原生 memory 文件按最新修订回填索引（内容在 vault 对象里）。
/// `force` 里的文件即使已有 fts 行也强制重建（merge 更新过 current_hash，旧行是
/// 旧内容）。对象缺失/非 UTF-8 跳过——与 memfiles collect 的"留到下一轮"口径
/// 一致；返回实际（重）建了索引的条数。
fn backfill_memory_fts(conn: &Connection, home: &Path, force: &[i64]) -> Result<u64> {
    // 索引以文件的当前修订为准：合并补进来的历史修订行 id 更大，但不是当前内容
    let mut targets: Vec<(i64, String)> = conn
        .prepare(
            "SELECT id, current_hash FROM memory_files f
             WHERE NOT EXISTS (SELECT 1 FROM memory_fts WHERE rowid = f.id)",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for &fid in force {
        let hash: Option<String> = conn
            .query_row(
                "SELECT current_hash FROM memory_files WHERE id = ?1",
                params![fid],
                |r| r.get(0),
            )
            .optional()?;
        targets.extend(hash.map(|h| (fid, h)));
    }
    let mut done = 0u64;
    for (fid, hash) in targets {
        match vault::read_object(home, &hash, false) {
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    db::set_memory_fts(conn, fid, text)?;
                    done += 1;
                }
                Err(_) => continue,
            },
            Err(_) => continue,
        }
    }
    Ok(done)
}

/// bundle 库某表的列名；旧版本 bundle 可能缺列或缺整张表（空列表）。
fn src_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut st = conn.prepare(&format!("PRAGMA src.table_info({table})"))?;
    let cols = st.query_map([], |r| r.get::<_, String>(1))?;
    Ok(cols.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn known_source_table(name: &str) -> bool {
    const TABLES: &[&str] = &[
        "projects",
        "sessions",
        "messages",
        "source_files",
        "vault_lines",
        "handoffs",
        "memories",
        "session_artifacts",
        "cross_checks",
        "session_links",
        "session_uuids",
        "memory_files",
        "memory_revisions",
        "purged_sources",
        "gc_pending",
        "usage_log",
        "work_meta",
        "change_log",
        "tasks",
        "task_events",
        "task_state_proposals",
        "task_sessions",
        "task_claims",
        "task_links",
        "task_verifications",
        "task_steps",
        "artifact_events",
        "artifact_occurrences",
        "memory_events",
        "project_record_pins",
        "labels",
        "memory_labels",
        "merge_conflicts",
        "sqlite_sequence",
        "sqlite_stat1",
        "sqlite_stat4",
    ];
    if TABLES.contains(&name) {
        return true;
    }
    [
        "messages_fts",
        "messages_tools_fts",
        "memories_fts",
        "memory_fts",
    ]
    .iter()
    .any(|base| {
        name == *base
            || [
                "data", "idx", "docsize", "config", "content", "segments", "segdir", "stat",
            ]
            .iter()
            .any(|suffix| name == format!("{base}_{suffix}"))
    })
}

fn validate_source_tables(conn: &Connection, schema: &str) -> Result<()> {
    let sql = format!("SELECT name FROM {schema}.sqlite_master WHERE type='table'");
    let mut stmt = conn.prepare(&sql)?;
    let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for name in names {
        let name = name?;
        anyhow::ensure!(
            known_source_table(&name),
            "bundle 含无法识别的额外表 {name}，拒绝恢复或合并"
        );
    }
    let mut objects = conn.prepare(&format!(
        "SELECT type,name FROM {schema}.sqlite_master WHERE type IN ('trigger','view')"
    ))?;
    let objects = objects
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (kind, name) in objects {
        let safe = if kind == "view" {
            name == "work_events"
        } else if name.starts_with("work_") {
            name.ends_with("_ai") || name.ends_with("_au") || name.ends_with("_ad")
        } else {
            [
                "messages_ai",
                "messages_ad",
                "messages_tools_ai",
                "messages_tools_ad",
                "memories_ai",
                "memories_ad",
                "memories_au",
            ]
            .contains(&name.as_str())
        };
        anyhow::ensure!(safe, "bundle 含无法识别的{kind} {name}，拒绝恢复或合并");
    }
    Ok(())
}

fn session_message_pointer_coords(
    conn: &Connection,
    table: &str,
    session_id: &str,
) -> Result<Vec<(String, i64, String, i64, i64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, m.id, m.session_id, m.line_no, m.ord
         FROM {table} e JOIN messages m ON e.source_message_id=m.id WHERE m.session_id=?1"
    ))?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn remap_session_message_pointers(
    conn: &Connection,
    table: &str,
    coords: Vec<(String, i64, String, i64, i64)>,
) -> Result<()> {
    for (id, _old, sid, line, ord) in coords {
        let new_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM messages WHERE session_id=?1 AND line_no=?2 AND ord=?3",
                params![sid, line, ord],
                |row| row.get(0),
            )
            .optional()?;
        conn.execute(
            &format!("UPDATE {table} SET source_message_id=?2 WHERE id=?1"),
            params![id, new_id],
        )?;
    }
    Ok(())
}

/// Merge a bundle's DB snapshot into an existing library (ATTACH + upserts).
///
/// 规则：
/// - projects 按 path 合并（记录 id 重映射，所有带 project_id 的表一律走它）；
/// - session 已存在且 bundle 侧不更新：整体跳过；bundle 更新（消息更多）：
///   该会话整体替换（消息重插拿新 rowid，FTS 由触发器维护）；
/// - 小表按主键/唯一约束 INSERT OR IGNORE；source_files offset 取 max；
/// - memory_revisions 无唯一约束，按 (file_id, hash) 查重后插入（file_id 重映射）。
fn merge_db(target: &Connection, src_db: &Path, home: &Path) -> Result<Value> {
    target.execute(
        "ATTACH DATABASE ?1 AS src",
        params![src_db.to_string_lossy().as_ref()],
    )?;
    let r = (|| -> Result<Value> {
        let tx = target.unchecked_transaction()?;
        validate_source_tables(&tx, "src")?;

        // projects: merge by path, build old->new id map
        let mut project_map: HashMap<i64, i64> = HashMap::new();
        {
            // archived_at 是 schema v11 的列；更早的 bundle 没有它，按 NULL（活跃）并入。
            // src 有值时并入目标且 MAX 取新：任何一侧已归档都保持归档——merge 不复活。
            let arch_col = if src_columns(&tx, "projects")?
                .iter()
                .any(|c| c == "archived_at")
            {
                "archived_at"
            } else {
                "NULL"
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT id, path, name, {arch_col} FROM src.projects"
            ))?;
            let rows: Vec<(i64, String, String, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (old_id, path, name, archived_at) in rows {
                let new_id = db::upsert_project(&tx, &path, &name)?;
                if let Some(ts) = archived_at {
                    tx.execute(
                        "UPDATE projects SET archived_at = MAX(COALESCE(archived_at, ''), ?1) WHERE id = ?2",
                        params![ts, new_id],
                    )?;
                }
                project_map.insert(old_id, new_id);
            }
        }

        // sessions: skip when target's copy is at least as new, else replace wholesale
        let mut sessions_added = 0u64;
        // 被替换会话上既有 memory 指针的坐标（memory id, 旧消息 id, sid, line, ord）
        // ——按 memory id（TEXT，mem_ 前缀）记录：旧消息 rowid 会被重插复用，
        // 按它做 UPDATE 选择条件会连环错挂（终审三轮）
        let mut replaced_pointer_coords: Vec<(String, i64, String, i64, i64)> = Vec::new();
        let mut replaced_verification_coords: Vec<(String, i64, String, i64, i64)> = Vec::new();
        let mut replaced_memory_event_coords: Vec<(String, i64, String, i64, i64)> = Vec::new();
        let mut sessions_replaced = 0u64;
        let mut sessions_skipped = 0u64;
        let mut replaced_ids: Vec<String> = Vec::new();
        {
            // deleted_at 是 schema v4 的列；v3 bundle 没有它，按 NULL（未删除）并入。
            // compact_line_no 是 v10 的列；更早的 bundle 没有它，按 NULL（无压缩点）并入。
            let (del_col, compact_col) = {
                let cols = src_columns(&tx, "sessions")?;
                (
                    if cols.iter().any(|c| c == "deleted_at") {
                        "deleted_at"
                    } else {
                        "NULL"
                    },
                    if cols.iter().any(|c| c == "compact_line_no") {
                        "compact_line_no"
                    } else {
                        "NULL"
                    },
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
                    .query_row(
                        "SELECT message_count FROM sessions WHERE id = ?1",
                        params![sid],
                        |r| r.get(0),
                    )
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
                        replaced_verification_coords.extend(session_message_pointer_coords(
                            &tx,
                            "task_verifications",
                            sid,
                        )?);
                        replaced_memory_event_coords.extend(session_message_pointer_coords(
                            &tx,
                            "memory_events",
                            sid,
                        )?);
                        // 本机的谱系边与回收站状态不随替换丢失：替换只更新会话内容
                        let local_links: Vec<(String, String, String, Option<String>, String)> = {
                            let mut st = tx.prepare(
                                "SELECT child_session_id, parent_session_id, link_type, via_uuid, created_at
                                 FROM session_links WHERE child_session_id = ?1 OR parent_session_id = ?1",
                            )?;
                            let rows = st.query_map(params![sid], |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                            })?;
                            rows.collect::<std::result::Result<Vec<_>, _>>()?
                        };
                        let local_deleted_at: Option<String> = tx.query_row(
                            "SELECT deleted_at FROM sessions WHERE id = ?1",
                            params![sid],
                            |r| r.get(0),
                        )?;
                        db::replace_session_data(&tx, sid)?;
                        insert_session(&tx, &s, &project_map)?;
                        tx.execute(
                            "UPDATE sessions SET deleted_at = ?2 WHERE id = ?1",
                            params![sid, local_deleted_at],
                        )?;
                        for (child, parent, link_type, via, created) in &local_links {
                            tx.execute(
                                "INSERT OR IGNORE INTO session_links(child_session_id, parent_session_id, link_type, via_uuid, created_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5)",
                                params![child, parent, link_type, via, created],
                            )?;
                        }
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
        let message_columns = src_columns(&tx, "messages")?;
        let native_col = if message_columns.iter().any(|c| c == "native_tool_call_id") {
            "native_tool_call_id"
        } else {
            "NULL"
        };
        let success_col = if message_columns.iter().any(|c| c == "tool_success") {
            "tool_success"
        } else {
            "NULL"
        };
        for sid in &replaced_ids {
            let mut stmt = tx.prepare(&format!(
                "SELECT line_no, ord, kind, content, timestamp, uuid, {native_col}, {success_col}, id FROM src.messages
                 WHERE session_id = ?1 ORDER BY line_no, ord",
            ))?;
            #[allow(clippy::type_complexity)]
            let msgs: Vec<(
                i64,
                i64,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<bool>,
                i64,
            )> = stmt
                .query_map(params![sid], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            // 消息 rowid 重插后会变——记录旧→新映射，memories 的
            // source_message_id 必须跟着重映射（终审 blocker）
            for (
                line_no,
                ord,
                kind,
                content,
                timestamp,
                uuid,
                native_tool_call_id,
                tool_success,
                old_id,
            ) in &msgs
            {
                tx.execute(
                    "INSERT INTO messages(session_id, line_no, ord, kind, content, timestamp, uuid, native_tool_call_id, tool_success)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![sid, line_no, ord, kind, content, timestamp, uuid, native_tool_call_id, tool_success],
                )?;
                msg_id_map.insert(*old_id, tx.last_insert_rowid());
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
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
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
        let mut cross_checks_added = 0u64;
        if !src_columns(&tx, "cross_checks")?.is_empty() {
            let mut stmt = tx.prepare(
                "SELECT project_id, run_id, path, role, agent, provider, model, vendor,
                        visibility, isolation, identity, status, started_at, ended_at,
                        materials, header_hash, created_at, updated_at FROM src.cross_checks",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(json!({
                    "project_id": r.get::<_, i64>(0)?, "run_id": r.get::<_, String>(1)?,
                    "path": r.get::<_, String>(2)?, "role": r.get::<_, String>(3)?,
                    "agent": r.get::<_, String>(4)?, "provider": r.get::<_, String>(5)?,
                    "model": r.get::<_, String>(6)?, "vendor": r.get::<_, String>(7)?,
                    "visibility": r.get::<_, String>(8)?, "isolation": r.get::<_, String>(9)?,
                    "identity": r.get::<_, String>(10)?, "status": r.get::<_, String>(11)?,
                    "started_at": r.get::<_, String>(12)?, "ended_at": r.get::<_, String>(13)?,
                    "materials": r.get::<_, String>(14)?, "header_hash": r.get::<_, String>(15)?,
                    "created_at": r.get::<_, String>(16)?, "updated_at": r.get::<_, String>(17)?,
                }))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            for c in rows {
                let new_pid = c["project_id"]
                    .as_i64()
                    .and_then(|p| project_map.get(&p).copied());
                let Some(new_pid) = new_pid else { continue };
                cross_checks_added += tx.execute(
                    "INSERT OR IGNORE INTO cross_checks(project_id, run_id, path, role, agent, provider,
                            model, vendor, visibility, isolation, identity, status, started_at, ended_at,
                            materials, header_hash, created_at, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                    params![new_pid, c["run_id"].as_str(), c["path"].as_str(), c["role"].as_str(),
                        c["agent"].as_str(), c["provider"].as_str(), c["model"].as_str(),
                        c["vendor"].as_str(), c["visibility"].as_str(), c["isolation"].as_str(),
                        c["identity"].as_str(), c["status"].as_str(), c["started_at"].as_str(),
                        c["ended_at"].as_str(), c["materials"].as_str(), c["header_hash"].as_str(),
                        c["created_at"].as_str(), c["updated_at"].as_str()],
                )? as u64;
            }
        }
        let mut memories_added = 0u64;
        {
            let mut stmt = tx.prepare(
                "SELECT id, project_id, scope, type, content, status, source_session_id,
                        source_message_id, superseded_by, created_at, updated_at FROM src.memories",
            )?;
            let rows: Vec<(
                String,
                Option<i64>,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<i64>,
                Option<String>,
                String,
                String,
            )> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
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
                    params![
                        id, new_pid, scope, typ, content, status, ssid, new_smid, sup, created,
                        updated
                    ],
                )? as u64;
            }
        }
        // purged_sources 墓碑随迁（v6）：合并不带墓碑的话，目标库 import 会把
        // 已清除的会话复活（codex 八审）。v3-v5 旧 bundle 没这张表——探测后跳过。
        {
            if !src_columns(&tx, "purged_sources")?.is_empty() {
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
        // saw_response_item 是 v6 的列；更早的 bundle 按 0（未见过）并入，MAX 保留目标值
        let saw_col = if src_columns(&tx, "source_files")?
            .iter()
            .any(|c| c == "saw_response_item")
        {
            "saw_response_item"
        } else {
            "0"
        };
        tx.execute(
            &format!(
                "INSERT INTO source_files(path, agent, imported_bytes, line_count, cursor_text,
                                          saw_response_item, updated_at)
                 SELECT path, agent, imported_bytes, line_count, cursor_text, {saw_col}, updated_at
                 FROM src.source_files WHERE 1=1
                 ON CONFLICT(agent, path) DO UPDATE SET
                   imported_bytes = MAX(source_files.imported_bytes, excluded.imported_bytes),
                   line_count = MAX(source_files.line_count, excluded.line_count),
                   cursor_text = COALESCE(excluded.cursor_text, source_files.cursor_text),
                   saw_response_item = MAX(source_files.saw_response_item, excluded.saw_response_item),
                   updated_at = MAX(source_files.updated_at, excluded.updated_at)"
            ),
            [],
        )?;
        // handoffs: project_id 重映射后追加（id 新分配）。同一 bundle 重复 --merge
        // 是常见误操作，按 (project_id, session_id, title, created_at) 查重跳过——
        // latest_handoff 按 id DESC 取，重复行会翻倍进卷宗
        let mut handoffs_added = 0u64;
        let mut handoff_id_map: HashMap<i64, i64> = HashMap::new();
        let mut fts_stale: Vec<i64> = Vec::new();
        {
            let handoff_cols = src_columns(&tx, "handoffs")?;
            let uuid_col = if handoff_cols.iter().any(|c| c == "uuid") {
                "uuid"
            } else {
                "NULL"
            };
            let task_col = if handoff_cols.iter().any(|c| c == "task_id") {
                "task_id"
            } else {
                "NULL"
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT id, project_id, session_id, title, done, state, decisions, files_changed,
                        open_issues, next_steps, created_at, {uuid_col}, {task_col} FROM src.handoffs",
            ))?;
            #[allow(clippy::type_complexity)]
            let rows: Vec<(
                i64,
                Option<i64>,
                Option<String>,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
            )> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                        r.get(12)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (
                old_id,
                pid,
                sid,
                title,
                done,
                state,
                decisions,
                files,
                issues,
                next,
                created,
                uuid,
                task_id,
            ) in rows
            {
                let new_pid = pid.and_then(|p| project_map.get(&p).copied());
                let Some(new_pid) = new_pid else { continue };
                let dup: Option<i64> = if let Some(uuid) = uuid.as_deref() {
                    tx.query_row("SELECT id FROM handoffs WHERE uuid=?1", [uuid], |r| {
                        r.get(0)
                    })
                    .optional()?
                } else {
                    tx.query_row(
                        "SELECT id FROM handoffs WHERE project_id = ?1 AND session_id IS ?2
                           AND title = ?3 AND created_at = ?4",
                        params![new_pid, sid, title, created],
                        |r| r.get(0),
                    )
                    .optional()?
                };
                if let Some(existing) = dup {
                    handoff_id_map.insert(old_id, existing);
                    continue;
                }
                tx.execute(
                    "INSERT INTO handoffs(uuid, project_id, session_id, task_id, title, done, state, decisions,
                                          files_changed, open_issues, next_steps, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![uuid, new_pid, sid, task_id, title, done, state, decisions, files, issues, next, created],
                )?;
                handoff_id_map.insert(old_id, tx.last_insert_rowid());
                handoffs_added += 1;
            }
        }
        // memory_files: 按 (agent, path) 合并；revision_uuid 是跨机修订身份。
        let mut memory_revisions_added = 0u64;
        let mut file_map: HashMap<i64, i64> = HashMap::new();
        let mut revision_id_map: HashMap<i64, i64> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id, agent, scope, path, current_hash, updated_at FROM src.memory_files",
            )?;
            let rows: Vec<(i64, String, String, String, String, String)> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
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
            let revision_uuid_col = if src_columns(&tx, "memory_revisions")?
                .iter()
                .any(|c| c == "revision_uuid")
            {
                "revision_uuid"
            } else {
                "NULL"
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT id, file_id, hash, size, captured_at, {revision_uuid_col} FROM src.memory_revisions"
            ))?;
            let rows: Vec<(i64, i64, String, i64, String, Option<String>)> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (old_id, old_fid, hash, size, captured, revision_uuid) in rows {
                let Some(new_fid) = file_map.get(&old_fid).copied() else {
                    continue;
                };
                let dup: Option<i64> = if let Some(uuid) = revision_uuid.as_deref() {
                    tx.query_row(
                        "SELECT id FROM memory_revisions WHERE revision_uuid=?1",
                        [uuid],
                        |r| r.get(0),
                    )
                    .optional()?
                } else {
                    tx.query_row(
                        "SELECT id FROM memory_revisions WHERE file_id=?1 AND hash=?2 AND captured_at=?3",
                        params![new_fid, hash, captured], |r| r.get(0),
                    ).optional()?
                };
                if let Some(existing) = dup {
                    revision_id_map.insert(old_id, existing);
                    continue;
                }
                tx.execute(
                    "INSERT INTO memory_revisions(file_id, hash, size, captured_at, revision_uuid) VALUES (?1,?2,?3,?4,?5)",
                    params![new_fid, hash, size, captured, revision_uuid],
                )?;
                revision_id_map.insert(old_id, tx.last_insert_rowid());
                memory_revisions_added += 1;
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
        remap_session_message_pointers(
            &tx,
            "task_verifications",
            std::mem::take(&mut replaced_verification_coords),
        )?;
        remap_session_message_pointers(
            &tx,
            "memory_events",
            std::mem::take(&mut replaced_memory_event_coords),
        )?;

        // 墓碑让位于显式迁移（终审 blocker）：bundle 带来的会话若在目标库有
        // 墓碑（曾 purge），保留会让"会话存在但源增量被阻断"的矛盾态——
        // 迁移意图优先，删墓碑放行后续导入。
        tx.execute(
            "DELETE FROM purged_sources WHERE session_id IN (SELECT id FROM src.sessions)",
            [],
        )?;

        let fts_backfilled = backfill_memory_fts(&tx, home, &fts_stale)?;
        // 旧版本备份带进来的产物时间未经 v13 修正，并入后补做（幂等）
        db::fix_imported_artifact_times(&tx)?;
        let work = merge_work_assets(
            &tx,
            &project_map,
            &msg_id_map,
            &handoff_id_map,
            &file_map,
            &revision_id_map,
        )?;
        let previous_generation = crate::work::database_generation(&tx)?;
        let database_generation = crate::work::regenerate_generation(&tx)?;
        tx.execute(
            "INSERT INTO change_log(entity_table,entity_key,operation,committed_at) VALUES('merge',?1,'merge',?2)",
            params![database_generation, crate::now_iso()],
        )?;
        tx.commit()?;
        Ok(json!({
            "sessions_added": sessions_added,
            "sessions_replaced": sessions_replaced,
            "sessions_skipped": sessions_skipped,
            "messages_added": messages_added,
            "artifacts_added": artifacts_added,
            "cross_checks_added": cross_checks_added,
            "memories_added": memories_added,
            "handoffs_added": handoffs_added,
            "memory_revisions_added": memory_revisions_added,
            "memory_fts_stale": fts_stale,
            "memory_fts_backfilled": fts_backfilled,
            "work": work,
            "previous_generation": previous_generation,
            "database_generation": database_generation,
        }))
    })();
    target.execute("DETACH DATABASE src", [])?;
    r
}

fn insert_session(conn: &Connection, s: &Value, project_map: &HashMap<i64, i64>) -> Result<()> {
    let new_pid = s["project_id"]
        .as_i64()
        .and_then(|p| project_map.get(&p).copied());
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

fn task_value(conn: &Connection, schema: &str, id: &str) -> Result<Option<Value>> {
    let sql = format!("SELECT id,project_id,task_no,title,status,status_source,state_policy,origin,revision,created_at,updated_at,completed_at FROM {schema}.tasks WHERE id=?1");
    conn.query_row(&sql, [id], |r| {
        Ok(json!({
            "id":r.get::<_,String>(0)?,"project_id":r.get::<_,i64>(1)?,"task_no":r.get::<_,i64>(2)?,
            "title":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"status_source":r.get::<_,String>(5)?,
            "state_policy":r.get::<_,String>(6)?,"origin":r.get::<_,String>(7)?,"revision":r.get::<_,i64>(8)?,
            "created_at":r.get::<_,String>(9)?,"updated_at":r.get::<_,String>(10)?,"completed_at":r.get::<_,Option<String>>(11)?,
        }))
    }).optional().map_err(Into::into)
}

fn task_event_values(
    conn: &Connection,
    schema: &str,
    task_id: &str,
    applied_only: bool,
) -> Result<Vec<Value>> {
    let sql = format!("SELECT id,task_id,parent_event_id,session_id,actor,change_kind,old_value,new_value,effect,occurred_at,observed_at,created_at FROM {schema}.task_events WHERE task_id=?1 {} ORDER BY rowid", if applied_only { "AND effect='applied'" } else { "" });
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([task_id], |r| {
        Ok(json!({
            "id":r.get::<_,String>(0)?,"task_id":r.get::<_,String>(1)?,"parent_event_id":r.get::<_,Option<String>>(2)?,
            "session_id":r.get::<_,Option<String>>(3)?,"actor":r.get::<_,String>(4)?,"change_kind":r.get::<_,String>(5)?,
            "old_value":r.get::<_,Option<String>>(6)?,"new_value":r.get::<_,Option<String>>(7)?,"effect":r.get::<_,String>(8)?,
            "occurred_at":r.get::<_,String>(9)?,"observed_at":r.get::<_,String>(10)?,"created_at":r.get::<_,String>(11)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn ordered_applied_chain(events: &[Value]) -> std::result::Result<Vec<Value>, String> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_id = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots = Vec::new();
    for event in events {
        let id = event["id"].as_str().unwrap_or_default().to_string();
        if by_id.insert(id.clone(), event).is_some() {
            return Err(format!("重复事件 id {id}"));
        }
        if let Some(parent) = event["parent_event_id"].as_str() {
            children.entry(parent.to_string()).or_default().push(id);
        } else {
            roots.push(id);
        }
    }
    for parent in children.keys() {
        if !by_id.contains_key(parent) {
            return Err(format!("缺少父事件 {parent}"));
        }
    }
    if roots.len() != 1 || children.values().any(|items| items.len() > 1) {
        return Err("applied 事件存在多个根或分叉头".into());
    }
    let mut ordered = Vec::new();
    let mut current = roots.remove(0);
    loop {
        let Some(event) = by_id.get(&current) else {
            return Err("事件链断裂".into());
        };
        ordered.push((*event).clone());
        let next = children
            .get(&current)
            .and_then(|items| items.first())
            .cloned();
        let Some(next) = next else { break };
        current = next;
    }
    if ordered.len() != events.len() {
        return Err("applied 事件存在环或 disconnected 链".into());
    }
    Ok(ordered)
}

fn replay_task(mut current: Value, chain: &[Value]) -> Result<Value> {
    let mut revision = 1i64;
    for event in chain {
        let kind = event["change_kind"].as_str().unwrap_or("");
        match kind {
            "created" => {
                current["title"] = event["new_value"].clone();
                current["created_at"] = event["occurred_at"].clone();
                current["status"] = json!("open");
                current["completed_at"] = Value::Null;
            }
            "title_changed" => {
                current["title"] = event["new_value"].clone();
                revision += 1;
            }
            "merge_resolved" => {
                let change: Value =
                    serde_json::from_str(event["new_value"].as_str().unwrap_or("{}"))
                        .context("merge_resolved 事件缺少任务快照")?;
                for field in [
                    "title",
                    "status",
                    "status_source",
                    "state_policy",
                    "revision",
                    "created_at",
                    "completed_at",
                ] {
                    current[field] = change[field].clone();
                }
                revision = change["revision"].as_i64().unwrap_or(revision);
            }
            "updated" => {
                let change: Value =
                    serde_json::from_str(event["new_value"].as_str().unwrap_or("{}"))
                        .context("task updated 事件缺少合法的新状态")?;
                current["title"] = change["title"].clone();
                current["status"] = change["status"].clone();
                current["completed_at"] = if change["status"] == "completed" {
                    event["occurred_at"].clone()
                } else {
                    Value::Null
                };
                revision += 1;
            }
            "status_changed" | "archived" => {
                current["status"] = event["new_value"].clone();
                current["completed_at"] = if event["new_value"] == "completed" {
                    event["occurred_at"].clone()
                } else {
                    Value::Null
                };
                revision += 1;
            }
            _ => continue,
        }
        if matches!(
            kind,
            "status_changed" | "archived" | "updated" | "merge_resolved"
        ) {
            let user = event["actor"] == "user";
            current["status_source"] = json!(if user {
                "user_confirmed"
            } else {
                "agent_reported"
            });
            current["state_policy"] = json!(if user { "user_locked" } else { "agent_managed" });
        }
    }
    if let Some(head) = chain.last() {
        current["updated_at"] = head["occurred_at"].clone();
    }
    current["revision"] = json!(revision);
    Ok(current)
}

fn record_merge_conflict(
    conn: &Connection,
    kind: &str,
    entity_key: &str,
    local: &Value,
    incoming: &Value,
) -> Result<u64> {
    let local_text = serde_json::to_string(local)?;
    let incoming_text = serde_json::to_string(incoming)?;
    let id = format!(
        "merge_{}",
        vault::hash_bytes(
            format!("{kind}\0{entity_key}\0{local_text}\0{incoming_text}").as_bytes()
        )
    );
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO merge_conflicts(id,kind,entity_key,local_value,incoming_value,status,created_at)
         VALUES(?1,?2,?3,?4,?5,'pending',?6)",
        params![id, kind, entity_key, local_text, incoming_text, crate::now_iso()],
    )?;
    Ok(inserted as u64)
}

fn direct_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn work_restore_plan(source: &Connection, target: Option<&Connection>) -> Result<Value> {
    let empty = || json!({"task_forks":[],"task_alias_remaps":[],"relation_conflicts":[]});
    let Some(target) = target else {
        return Ok(empty());
    };
    if !direct_table_exists(source, "tasks")? || !direct_table_exists(target, "tasks")? {
        return Ok(empty());
    }
    let mut project_map = HashMap::new();
    let mut stmt = source.prepare("SELECT id,path FROM projects")?;
    let projects = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (old, path) in projects {
        if let Some(new) = target
            .query_row("SELECT id FROM projects WHERE path=?1", [path], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            project_map.insert(old, new);
        }
    }
    let mut aliases = Vec::new();
    let mut forks = Vec::new();
    let mut next_alias: HashMap<i64, i64> = HashMap::new();
    let tasks: Vec<(String, i64, i64)> = source
        .prepare("SELECT id,project_id,task_no FROM tasks ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, old_pid, task_no) in tasks {
        let Some(pid) = project_map.get(&old_pid).copied() else {
            continue;
        };
        let target_task = task_value(target, "main", &id)?;
        if target_task.is_none() {
            let owner: Option<String> = target
                .query_row(
                    "SELECT id FROM tasks WHERE project_id=?1 AND task_no=?2",
                    params![pid, task_no],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(owner) = owner {
                let proposed = next_alias.entry(pid).or_insert_with(|| 0);
                if *proposed == 0 {
                    *proposed = target.query_row(
                        "SELECT COALESCE(MAX(task_no),0)+1 FROM tasks WHERE project_id=?1",
                        [pid],
                        |r| r.get(0),
                    )?;
                }
                aliases.push(json!({"task_id":id,"project_id":pid,"source_task_no":task_no,"target_task_no":*proposed,"occupied_by":owner}));
                *proposed += 1;
            }
            continue;
        }
        let mut events = task_event_values(target, "main", &id, true)?;
        events.extend(task_event_values(source, "main", &id, true)?);
        events.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        events.dedup_by(|a, b| a["id"] == b["id"]);
        if let Err(reason) = ordered_applied_chain(&events) {
            forks.push(json!({"task_id":id,"reason":reason}));
        }
    }
    let mut relations = Vec::new();
    if direct_table_exists(source, "task_sessions")?
        && direct_table_exists(target, "task_sessions")?
    {
        let rows: Vec<(String, String, String, String, String)> = source
            .prepare("SELECT task_id,session_id,relation,evidence,state FROM task_sessions")?
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (task, session, relation, evidence, state) in rows {
            let local: Option<String> = target.query_row("SELECT state FROM task_sessions WHERE task_id=?1 AND session_id=?2 AND relation=?3 AND evidence=?4",
                params![task,session,relation,evidence],|r|r.get(0)).optional()?;
            if local.as_ref().is_some_and(|local| local != &state) {
                relations.push(json!({"kind":"task_session","task_id":task,"session_id":session,"relation":relation,"evidence":evidence,"local_state":local,"incoming_state":state}));
            }
        }
    }
    if direct_table_exists(source, "task_links")? && direct_table_exists(target, "task_links")? {
        let rows: Vec<(String, String, String, String, String)> = source
            .prepare(
                "SELECT source_task_id,target_task_id,relation,evidence,status FROM task_links",
            )?
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (source_task, target_task, relation, evidence, status) in rows {
            let local: Option<String> = target.query_row("SELECT status FROM task_links WHERE source_task_id=?1 AND target_task_id=?2 AND relation=?3 AND evidence=?4",
                params![source_task,target_task,relation,evidence],|r|r.get(0)).optional()?;
            if local.as_ref().is_some_and(|local| local != &status) {
                relations.push(json!({"kind":"task_link","source_task_id":source_task,"target_task_id":target_task,"relation":relation,"evidence":evidence,"local_status":local,"incoming_status":status}));
            }
        }
    }
    Ok(json!({"task_forks":forks,"task_alias_remaps":aliases,"relation_conflicts":relations}))
}

fn merge_work_assets(
    tx: &Connection,
    project_map: &HashMap<i64, i64>,
    msg_id_map: &HashMap<i64, i64>,
    handoff_id_map: &HashMap<i64, i64>,
    file_map: &HashMap<i64, i64>,
    revision_id_map: &HashMap<i64, i64>,
) -> Result<Value> {
    let mut tasks_added = 0u64;
    let mut alias_remaps = 0u64;
    let mut events_added = 0u64;
    let mut task_forks = 0u64;
    let mut relation_conflicts = 0u64;
    let mut claim_conflicts = 0u64;
    if !src_columns(tx, "tasks")?.is_empty() {
        let ids: Vec<String> = tx
            .prepare("SELECT id FROM src.tasks ORDER BY id")?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut task_pairs = Vec::new();
        for id in ids {
            let incoming = task_value(tx, "src", &id)?.context("src task disappeared")?;
            let old_pid = incoming["project_id"].as_i64().context("task project_id")?;
            let new_pid = project_map
                .get(&old_pid)
                .copied()
                .context("task 缺少项目重映射")?;
            let mut local = task_value(tx, "main", &id)?;
            if let Some(current) = &local {
                if current["project_id"] != new_pid {
                    relation_conflicts +=
                        record_merge_conflict(tx, "task_project", &id, current, &incoming)?;
                }
            } else {
                let mut task_no = incoming["task_no"].as_i64().context("task_no")?;
                let alias_owner: Option<String> = tx
                    .query_row(
                        "SELECT id FROM tasks WHERE project_id=?1 AND task_no=?2",
                        params![new_pid, task_no],
                        |row| row.get(0),
                    )
                    .optional()?;
                if alias_owner.is_some() {
                    task_no = tx.query_row(
                        "SELECT COALESCE(MAX(task_no),0)+1 FROM tasks WHERE project_id=?1",
                        [new_pid],
                        |row| row.get(0),
                    )?;
                    alias_remaps += 1;
                }
                tx.execute(
                    "INSERT INTO tasks(id,project_id,task_no,title,status,status_source,state_policy,origin,revision,created_at,updated_at,completed_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![id, new_pid, task_no, incoming["title"].as_str(), incoming["status"].as_str(),
                        incoming["status_source"].as_str(), incoming["state_policy"].as_str(), incoming["origin"].as_str(),
                        incoming["revision"].as_i64(), incoming["created_at"].as_str(), incoming["updated_at"].as_str(),
                        incoming["completed_at"].as_str()],
                )?;
                tasks_added += 1;
                local = task_value(tx, "main", &id)?;
            }
            task_pairs.push((id, local.context("local task missing")?, incoming));
        }

        for (id, _, _) in &task_pairs {
            for event in task_event_values(tx, "src", id, false)? {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM task_events WHERE id=?1)",
                    [event["id"].as_str().unwrap()],
                    |row| row.get(0),
                )?;
                if exists {
                    tx.execute("UPDATE task_events SET effect=CASE WHEN effect='superseded' OR ?2='superseded' THEN 'superseded' ELSE effect END WHERE id=?1",
                        params![event["id"].as_str(),event["effect"].as_str()])?;
                    continue;
                }
                tx.execute(
                    "INSERT INTO task_events(id,task_id,parent_event_id,session_id,actor,change_kind,old_value,new_value,effect,occurred_at,observed_at,created_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![event["id"].as_str(), id, event["parent_event_id"].as_str(), event["session_id"].as_str(),
                        event["actor"].as_str(), event["change_kind"].as_str(), event["old_value"].as_str(), event["new_value"].as_str(),
                        event["effect"].as_str(), event["occurred_at"].as_str(), event["observed_at"].as_str(), event["created_at"].as_str()],
                )?;
                events_added += 1;
            }
        }
        for (id, local, incoming) in task_pairs {
            let events = task_event_values(tx, "main", &id, true)?;
            match ordered_applied_chain(&events) {
                Ok(chain) if !chain.is_empty() => {
                    let replayed = replay_task(local, &chain)?;
                    tx.execute(
                        "UPDATE tasks SET title=?2,status=?3,status_source=?4,state_policy=?5,revision=?6,created_at=?7,updated_at=?8,completed_at=?9 WHERE id=?1",
                        params![id, replayed["title"].as_str(), replayed["status"].as_str(), replayed["status_source"].as_str(),
                            replayed["state_policy"].as_str(), replayed["revision"].as_i64(), replayed["created_at"].as_str(),
                            replayed["updated_at"].as_str(), replayed["completed_at"].as_str()],
                    )?;
                }
                Ok(_) => {}
                Err(reason) => {
                    task_forks += 1;
                    let local_report = json!({"task":local,"reason":reason});
                    let incoming_report =
                        json!({"task":incoming,"events":task_event_values(tx,"src",&id,true)?});
                    record_merge_conflict(
                        tx,
                        "task_event_fork",
                        &id,
                        &local_report,
                        &incoming_report,
                    )?;
                }
            }
        }
    }

    if !src_columns(tx, "task_state_proposals")?.is_empty() {
        tx.execute(
            "INSERT OR IGNORE INTO task_state_proposals(id,task_id,proposed_status,source_event_id,status,created_at,resolved_at)
             SELECT id,task_id,proposed_status,source_event_id,status,created_at,resolved_at FROM src.task_state_proposals", [],
        )?;
    }
    if !src_columns(tx, "task_sessions")?.is_empty() {
        let rows: Vec<(String,String,String,String,String,String)> = tx.prepare(
            "SELECT task_id,session_id,relation,evidence,state,created_at FROM src.task_sessions"
        )?.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (task_id, sid, relation, evidence, state, created) in rows {
            let local: Option<(String,String)> = tx.query_row(
                "SELECT state,created_at FROM task_sessions WHERE task_id=?1 AND session_id=?2 AND relation=?3 AND evidence=?4",
                params![task_id,sid,relation,evidence], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional()?;
            let incoming = json!({"state":state,"created_at":created});
            let Some((local_state, local_created)) = local else {
                tx.execute("INSERT INTO task_sessions(task_id,session_id,relation,evidence,state,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![task_id,sid,relation,evidence,state,created])?;
                continue;
            };
            if local_state == state {
                continue;
            }
            let local_value = json!({"state":local_state,"created_at":local_created});
            let local_user = local_state == "rejected"
                || (local_state == "active" && evidence == "user_confirmed");
            let incoming_user =
                state == "rejected" || (state == "active" && evidence == "user_confirmed");
            if local_user && incoming_user {
                relation_conflicts += record_merge_conflict(
                    tx,
                    "task_session_decision",
                    &format!("{task_id}:{sid}:{relation}:{evidence}"),
                    &local_value,
                    &incoming,
                )?;
            } else if state == "rejected" {
                tx.execute("UPDATE task_sessions SET state='rejected' WHERE task_id=?1 AND session_id=?2 AND relation=?3 AND evidence=?4",
                    params![task_id,sid,relation,evidence])?;
            }
        }
    }
    if !src_columns(tx, "task_claims")?.is_empty() {
        let rows: Vec<(String,String,String,String,String,String,String,Option<String>,Option<String>)> = tx.prepare(
            "SELECT claim_id,token_hash,task_id,relation,status,created_at,expires_at,claimed_session_id,native_tool_call_id FROM src.task_claims"
        )?.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, token, task_id, relation, status, created, expires, claimed, native) in rows {
            let local: Option<(String,String,String,Option<String>,Option<String>)> = tx.query_row(
                "SELECT token_hash,status,created_at,claimed_session_id,native_tool_call_id FROM task_claims WHERE claim_id=?1",
                [&id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
            ).optional()?;
            let incoming = json!({"token_hash":token,"status":status,"claimed_session_id":claimed,"native_tool_call_id":native});
            let Some((local_token, local_status, local_created, local_claimed, local_native)) =
                local
            else {
                let token_owner: Option<String> = tx
                    .query_row(
                        "SELECT claim_id FROM task_claims WHERE token_hash=?1",
                        [&token],
                        |r| r.get(0),
                    )
                    .optional()?;
                if token_owner.is_some() {
                    claim_conflicts += record_merge_conflict(
                        tx,
                        "claim_identity",
                        &id,
                        &json!({"token_owner":token_owner}),
                        &incoming,
                    )?;
                    continue;
                }
                tx.execute("INSERT INTO task_claims(claim_id,token_hash,task_id,relation,status,created_at,expires_at,claimed_session_id,native_tool_call_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![id,token,task_id,relation,status,created,expires,claimed,native])?;
                continue;
            };
            let local_value = json!({"token_hash":local_token,"status":local_status,"claimed_session_id":local_claimed,"native_tool_call_id":local_native,"created_at":local_created});
            if local_status == status && local_claimed == claimed && local_native == native {
                continue;
            }
            if local_token != token {
                claim_conflicts +=
                    record_merge_conflict(tx, "claim_identity", &id, &local_value, &incoming)?;
            } else if local_status == "pending" && status != "pending" {
                tx.execute("UPDATE task_claims SET status=?2,claimed_session_id=?3,native_tool_call_id=?4 WHERE claim_id=?1",
                    params![id,status,claimed,native])?;
            } else if status != "pending" {
                claim_conflicts +=
                    record_merge_conflict(tx, "claim_decision", &id, &local_value, &incoming)?;
            }
        }
    }
    if !src_columns(tx, "task_links")?.is_empty() {
        let rows: Vec<(String,String,String,String,String,Option<f64>,String,Option<String>)> = tx.prepare(
            "SELECT source_task_id,target_task_id,relation,status,evidence,score,created_at,confirmed_at FROM src.task_links"
        )?.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (source, target, relation, status, evidence, score, created, confirmed) in rows {
            let local: Option<(String,Option<String>)> = tx.query_row(
                "SELECT status,confirmed_at FROM task_links WHERE source_task_id=?1 AND target_task_id=?2 AND relation=?3 AND evidence=?4",
                params![source,target,relation,evidence], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional()?;
            let Some((local_status, local_confirmed)) = local else {
                tx.execute("INSERT INTO task_links(source_task_id,target_task_id,relation,status,evidence,score,created_at,confirmed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![source,target,relation,status,evidence,score,created,confirmed])?;
                continue;
            };
            if local_status == status {
                continue;
            }
            // 用户证据或非 suggested 状态视为用户决定：本机不是而对方是时采用对方；
            // 双方都是用户决定且不同只记冲突，不覆盖本机
            let local_user = evidence == "user_confirmed" || local_status != "suggested";
            let incoming_user = evidence == "user_confirmed" || status != "suggested";
            if !incoming_user {
                continue;
            }
            if !local_user {
                tx.execute("UPDATE task_links SET status=?5,confirmed_at=?6 WHERE source_task_id=?1 AND target_task_id=?2 AND relation=?3 AND evidence=?4",
                    params![source,target,relation,evidence,status,confirmed])?;
                continue;
            }
            let key = format!("{source}:{target}:{relation}:{evidence}");
            relation_conflicts += record_merge_conflict(
                tx,
                "task_link_decision",
                &key,
                &json!({"status":local_status,"confirmed_at":local_confirmed}),
                &json!({"status":status,"confirmed_at":confirmed}),
            )?;
        }
    }
    let remaining = merge_remaining_work_assets(
        tx,
        project_map,
        msg_id_map,
        handoff_id_map,
        file_map,
        revision_id_map,
    )?;
    Ok(json!({
        "tasks_added":tasks_added,"task_alias_remaps":alias_remaps,"task_events_added":events_added,
        "task_forks":task_forks,"relation_conflicts":relation_conflicts,"claim_conflicts":claim_conflicts,
        "assets":remaining,
    }))
}

fn merge_remaining_work_assets(
    tx: &Connection,
    project_map: &HashMap<i64, i64>,
    msg_id_map: &HashMap<i64, i64>,
    handoff_id_map: &HashMap<i64, i64>,
    file_map: &HashMap<i64, i64>,
    revision_id_map: &HashMap<i64, i64>,
) -> Result<Value> {
    let mut verifications = 0u64;
    if !src_columns(tx, "task_verifications")?.is_empty() {
        let rows: Vec<(String,String,String,String,String,Option<String>,Option<i64>,i64,String)> = tx.prepare(
            "SELECT id,task_id,kind,result,evidence,session_id,source_message_id,task_revision,created_at FROM src.task_verifications"
        )?.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        for (id, task_id, kind, result, evidence, sid, smid, revision, created) in rows {
            let mapped = smid.and_then(|mid| msg_id_map.get(&mid).copied());
            verifications += tx.execute("INSERT OR IGNORE INTO task_verifications(id,task_id,kind,result,evidence,session_id,source_message_id,task_revision,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id,task_id,kind,result,evidence,sid,mapped,revision,created])? as u64;
        }
    }
    let mut steps = 0u64;
    if !src_columns(tx, "task_steps")?.is_empty() {
        steps = tx.execute("INSERT OR IGNORE INTO task_steps(id,session_id,task_id,native_step_id,snapshot_event_id,content,status,source_line_no,ord,occurred_at,observed_at)
            SELECT id,session_id,task_id,native_step_id,snapshot_event_id,content,status,source_line_no,ord,occurred_at,observed_at FROM src.task_steps",[])? as u64;
    }
    let mut artifacts = 0u64;
    let mut artifact_map = HashMap::new();
    if !src_columns(tx, "artifact_events")?.is_empty() {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(String,String,Option<i64>,String,Option<String>,String,String,Option<String>,String,Option<String>,String,Option<String>,String,String,i64,i64,String)> = tx.prepare(
            "SELECT id,session_id,project_id,agent,native_tool_call_id,path,COALESCE(normalized_path,path),from_path,operation,tool,outcome,occurred_at,observed_at,time_quality,source_line_no,ord,metadata_json FROM src.artifact_events"
        )?.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?,r.get(13)?,r.get(14)?,r.get(15)?,r.get(16)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        for (
            id,
            sid,
            pid,
            agent,
            native,
            path,
            normalized,
            from,
            operation,
            tool,
            outcome,
            occurred,
            observed,
            quality,
            line,
            ord,
            metadata,
        ) in rows
        {
            let existing: Option<String> = tx
                .query_row("SELECT id FROM artifact_events WHERE id=?1", [&id], |r| {
                    r.get(0)
                })
                .optional()?;
            let existing = match existing {
                Some(id) => Some(id),
                None if native.is_some() && occurred.is_some() => tx.query_row("SELECT id FROM artifact_events WHERE agent=?1 AND native_tool_call_id=?2 AND path=?3 AND operation=?4 AND occurred_at=?5",
                    params![agent,native,path,operation,occurred],|r|r.get(0)).optional()?,
                None => tx.query_row("SELECT id FROM artifact_events WHERE session_id=?1 AND source_line_no=?2 AND ord=?3 AND path=?4 AND operation=?5",
                    params![sid,line,ord,path,operation],|r|r.get(0)).optional()?,
            };
            let mapped_id = if let Some(existing) = existing {
                existing
            } else {
                let new_pid = pid
                    .map(|old| {
                        project_map
                            .get(&old)
                            .copied()
                            .context("artifact 缺少项目重映射")
                    })
                    .transpose()?;
                tx.execute("INSERT INTO artifact_events(id,session_id,project_id,agent,native_tool_call_id,path,normalized_path,from_path,operation,tool,outcome,occurred_at,observed_at,time_quality,source_line_no,ord,metadata_json)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                    params![id,sid,new_pid,agent,native,path,normalized,from,operation,tool,outcome,occurred,observed,quality,line,ord,metadata])?;
                artifacts += 1;
                id.clone()
            };
            artifact_map.insert(id, mapped_id);
        }
    }
    let mut occurrences = 0u64;
    if !src_columns(tx, "artifact_occurrences")?.is_empty() {
        let rows: Vec<(String, String, i64, i64)> = tx
            .prepare(
                "SELECT artifact_id,session_id,source_line_no,ord FROM src.artifact_occurrences",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (old_id, sid, line, ord) in rows {
            let Some(id) = artifact_map.get(&old_id) else {
                continue;
            };
            occurrences += tx.execute("INSERT OR IGNORE INTO artifact_occurrences(artifact_id,session_id,source_line_no,ord) VALUES(?1,?2,?3,?4)",
                params![id,sid,line,ord])? as u64;
        }
    }
    let mut memory_events = 0u64;
    if !src_columns(tx, "memory_events")?.is_empty() {
        let rows: Vec<(String,String,Option<String>,String,Option<String>,Option<String>,String,Option<i64>)> = tx.prepare(
            "SELECT id,memory_id,session_id,change_kind,old_value,new_value,occurred_at,source_message_id FROM src.memory_events"
        )?.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        for (id, memory_id, sid, kind, old, new, occurred, smid) in rows {
            let mapped = smid.and_then(|mid| msg_id_map.get(&mid).copied());
            memory_events += tx.execute("INSERT OR IGNORE INTO memory_events(id,memory_id,session_id,change_kind,old_value,new_value,occurred_at,source_message_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id,memory_id,sid,kind,old,new,occurred,mapped])? as u64;
        }
    }
    let mut pins = 0u64;
    if !src_columns(tx, "project_record_pins")?.is_empty() {
        let rows: Vec<(i64, String, String, String)> = tx
            .prepare(
                "SELECT project_id,record_kind,record_id,created_at FROM src.project_record_pins",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (old_pid, kind, record, created) in rows {
            let Some(pid) = project_map.get(&old_pid).copied() else {
                continue;
            };
            let mapped = if kind == "handoff" {
                record
                    .strip_prefix("handoff:")
                    .and_then(|id| id.parse::<i64>().ok())
                    .and_then(|id| handoff_id_map.get(&id))
                    .map(|id| format!("handoff:{id}"))
                    .unwrap_or(record)
            } else if kind == "project_document" {
                let mut parts = record.split(':');
                match (
                    parts.next(),
                    parts.next().and_then(|v| v.parse::<i64>().ok()),
                    parts.next().and_then(|v| v.parse::<i64>().ok()),
                    parts.next(),
                ) {
                    (Some("file"), Some(file), Some(revision), None) => {
                        match (file_map.get(&file), revision_id_map.get(&revision)) {
                            (Some(file), Some(revision)) => format!("file:{file}:{revision}"),
                            _ => record,
                        }
                    }
                    _ => record,
                }
            } else {
                record
            };
            pins += tx.execute("INSERT OR IGNORE INTO project_record_pins(project_id,record_kind,record_id,created_at) VALUES(?1,?2,?3,?4)",
                params![pid,kind,mapped,created])? as u64;
        }
    }
    let mut label_map = HashMap::new();
    let mut labels = 0u64;
    if !src_columns(tx, "labels")?.is_empty() {
        let rows: Vec<(String, String, String, Option<i64>, String)> = tx
            .prepare("SELECT id,name,scope,project_id,created_at FROM src.labels")?
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, name, scope, pid, created) in rows {
            let new_pid = pid
                .map(|old| {
                    project_map
                        .get(&old)
                        .copied()
                        .context("label 缺少项目重映射")
                })
                .transpose()?;
            let existing: Option<String> = tx.query_row("SELECT id FROM labels WHERE id=?1 OR (name=?2 AND scope=?3 AND project_id IS ?4) ORDER BY id LIMIT 1",
                params![id,name,scope,new_pid],|r|r.get(0)).optional()?;
            let mapped = if let Some(existing) = existing {
                existing
            } else {
                tx.execute("INSERT INTO labels(id,name,scope,project_id,created_at) VALUES(?1,?2,?3,?4,?5)",params![id,name,scope,new_pid,created])?;
                labels += 1;
                id.clone()
            };
            label_map.insert(id, mapped);
        }
    }
    let mut memory_labels = 0u64;
    if !src_columns(tx, "memory_labels")?.is_empty() {
        let rows: Vec<(String, String)> = tx
            .prepare("SELECT memory_id,label_id FROM src.memory_labels")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (memory, old_label) in rows {
            let Some(label) = label_map.get(&old_label) else {
                continue;
            };
            memory_labels += tx.execute(
                "INSERT OR IGNORE INTO memory_labels(memory_id,label_id) VALUES(?1,?2)",
                params![memory, label],
            )? as u64;
        }
    }
    let mut imported_conflicts = 0u64;
    if !src_columns(tx, "merge_conflicts")?.is_empty() {
        // 源库的 local 是源机器本地值：换到目标机视角后，它是 incoming
        imported_conflicts = tx.execute("INSERT OR IGNORE INTO merge_conflicts(id,kind,entity_key,local_value,incoming_value,status,created_at)
            SELECT id,kind,entity_key,incoming_value,local_value,
                   CASE status WHEN 'resolved_local' THEN 'resolved_incoming' WHEN 'resolved_incoming' THEN 'resolved_local' ELSE status END,
                   created_at FROM src.merge_conflicts",[])? as u64;
    }
    if !src_columns(tx, "work_meta")?.is_empty() {
        tx.execute(
            "INSERT OR IGNORE INTO work_meta(key,value) SELECT key,value FROM src.work_meta
            WHERE key NOT IN ('database_generation','change_log_min_retained')",
            [],
        )?;
    }
    Ok(
        json!({"verifications":verifications,"steps":steps,"artifact_events":artifacts,"artifact_occurrences":occurrences,
        "memory_events":memory_events,"project_record_pins":pins,"labels":labels,"memory_labels":memory_labels,
        "merge_conflicts":imported_conflicts}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_paths_use_the_user_home_not_the_data_home() {
        let source_user = tempfile::tempdir().unwrap();
        let source_data = source_user.path().join(".yourmem");
        let target_user = tempfile::tempdir().unwrap();
        let target_data = target_user.path().join(".yourmem");
        let (_, report) = plan_config_path(
            "~/project",
            "extra_roots[0].path",
            ConfigPathKind::Directory,
            source_user.path(),
            &source_data,
            target_user.path(),
            &target_data,
            &HashMap::new(),
        );
        assert_eq!(
            report["candidate"],
            json!(target_user.path().join("project"))
        );
        assert_eq!(report["status"], "remap_available");
    }
}
