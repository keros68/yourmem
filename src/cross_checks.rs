//! ai-cross 项目留痕的只读元数据采集。只读取 `.dispatch/*.md` 的 YAML 头，
//! 不保存任务正文或模型输出，也不根据输出推断核对结论。

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::{db, project_docs, vault};

const MAX_HEADER_BYTES: u64 = 64 * 1024;

#[derive(Default)]
pub struct CollectOutcome {
    pub files_seen: usize,
    pub files_updated: usize,
    pub files_removed: usize,
}

#[derive(Debug, Default, PartialEq)]
struct TraceMeta {
    run_id: String,
    role: String,
    agent: String,
    provider: String,
    model: String,
    vendor: String,
    visibility: String,
    isolation: String,
    identity: String,
    status: String,
    started_at: String,
    ended_at: String,
    materials: Vec<String>,
}

fn scalar(raw: &str) -> String {
    let value = raw.trim();
    if value == "null" {
        return String::new();
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value)
            .unwrap_or_else(|_| value.trim_matches('"').to_string());
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value.to_string()
}

fn parse_header(header: &str) -> Option<TraceMeta> {
    let mut lines = header.lines();
    if lines.next()?.trim_start_matches('\u{feff}').trim() != "---" {
        return None;
    }
    let mut values: HashMap<&str, Vec<String>> = HashMap::new();
    let mut closed = false;
    let mut in_materials = false;
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if in_materials && line.trim_start().starts_with("- ") {
            if let Some(items) = values.get_mut("materials") {
                items.push(scalar(line.trim_start()[2..].trim()));
            }
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.starts_with('#') || line.starts_with([' ', '\t']) {
            in_materials = false;
            continue;
        }
        in_materials = key == "materials" && raw.trim().is_empty();
        if in_materials {
            values.insert("materials", Vec::new());
        } else if key == "materials" {
            let raw = raw.trim();
            let items = serde_json::from_str::<Vec<String>>(raw).unwrap_or_default();
            values.insert("materials", items);
        } else {
            values.insert(key, vec![scalar(raw)]);
        }
    }
    if !closed
        || values
            .get("schema")
            .and_then(|v| v.first())
            .map(String::as_str)
            != Some("aicross-dispatch/1")
    {
        return None;
    }
    let get = |key: &str| {
        values
            .get(key)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default()
    };
    let meta = TraceMeta {
        run_id: get("run_id"),
        role: get("role"),
        agent: get("agent"),
        provider: get("provider"),
        model: get("model"),
        vendor: get("vendor"),
        visibility: get("visibility"),
        isolation: get("isolation"),
        identity: get("identity"),
        status: get("status"),
        started_at: get("started_at"),
        ended_at: get("ended_at"),
        materials: values.get("materials").cloned().unwrap_or_default(),
    };
    (!meta.run_id.is_empty()).then_some(meta)
}

fn read_trace(path: &Path) -> Option<(TraceMeta, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_HEADER_BYTES).read_to_end(&mut bytes).ok()?;
    let mut delimiters = 0;
    let mut end = None;
    let mut offset = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        offset += line.len();
        if line.trim_ascii() == b"---" {
            delimiters += 1;
            if delimiters == 2 {
                end = Some(offset);
                break;
            }
        }
    }
    let header = &bytes[..end?];
    let text = std::str::from_utf8(header).ok()?;
    let meta = parse_header(text)?;
    Some((meta, vault::hash_bytes(header)))
}

fn known_projects(conn: &Connection, home: &Path) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, archived_at FROM projects ORDER BY archived_at IS NOT NULL, id",
    )?;
    let projects = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut by_path = HashMap::new();
    for (id, path, _) in &projects {
        by_path
            .entry(db::path_key(path))
            .or_insert((*id, path.clone()));
    }
    fn push_root(
        roots: &mut Vec<(i64, String)>,
        seen: &mut HashSet<(i64, String)>,
        id: i64,
        root: String,
    ) {
        if seen.insert((id, db::path_key(&root))) {
            roots.push((id, root));
        }
    }
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |path: String, by_path: &HashMap<String, (i64, String)>| {
        if let Some((id, root)) = by_path.get(&db::path_key(&path)) {
            push_root(&mut roots, &mut seen, *id, root.clone());
        }
    };
    let mut stmt = conn.prepare("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL")?;
    let cwds = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for cwd in cwds {
        let (root, _) = db::project_root_for_conn(conn, &cwd)?;
        add(root, &by_path);
    }
    for (_, path, archived) in &projects {
        if archived.is_none() {
            add(path.clone(), &by_path);
        }
    }
    for (project, _) in project_docs::tracked(home) {
        add(project, &by_path);
    }
    // 项目合并后记录改挂父项目，但原文件仍在子目录；从记录自身恢复扫描根。
    let trace_paths = {
        let mut stmt = conn.prepare("SELECT DISTINCT project_id, path FROM cross_checks")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    for (id, path) in trace_paths {
        if let Some(root) = Path::new(&path).parent().and_then(Path::parent) {
            push_root(
                &mut roots,
                &mut seen,
                id,
                root.to_string_lossy().to_string(),
            );
        }
    }
    Ok(roots)
}

pub fn collect(conn: &Connection, home: &Path) -> Result<CollectOutcome> {
    let mut outcome = CollectOutcome::default();
    let known = known_projects(conn, home)?;
    for (project_id, root) in &known {
        let root_path = Path::new(root);
        if !root_path.is_dir() {
            continue;
        }
        let dispatch = root_path.join(".dispatch");
        let entries = match std::fs::read_dir(&dispatch) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        let mut disk_paths = HashSet::new();
        let mut traces = Vec::new();
        let mut complete = true;
        if let Some(entries) = entries {
            for entry in entries {
                let Ok(entry) = entry else {
                    complete = false;
                    continue;
                };
                let path = entry.path();
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                    || !path
                        .extension()
                        .map(|e| e.eq_ignore_ascii_case("md"))
                        .unwrap_or(false)
                {
                    continue;
                }
                let path_string = path.to_string_lossy().to_string();
                disk_paths.insert(path_string.clone());
                if let Some((meta, hash)) = read_trace(&path) {
                    traces.push((path_string, meta, hash));
                }
            }
        }
        let existing = {
            let mut stmt = conn.prepare("SELECT path FROM cross_checks WHERE project_id = ?1")?;
            let rows = stmt
                .query_map([project_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let tx = conn.unchecked_transaction()?;
        for (path, meta, hash) in traces {
            outcome.files_seen += 1;
            let old_hash = tx
                .query_row(
                    "SELECT header_hash FROM cross_checks WHERE project_id = ?1 AND path = ?2",
                    params![project_id, path],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if old_hash.as_deref() == Some(&hash) {
                continue;
            }
            let now = crate::now_iso();
            tx.execute(
                "INSERT INTO cross_checks(project_id, run_id, path, role, agent, provider, model, vendor,
                        visibility, isolation, identity, status, started_at, ended_at, materials,
                        header_hash, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17)
                 ON CONFLICT(project_id, path) DO UPDATE SET
                   run_id=excluded.run_id, role=excluded.role, agent=excluded.agent,
                   provider=excluded.provider, model=excluded.model, vendor=excluded.vendor,
                   visibility=excluded.visibility, isolation=excluded.isolation,
                   identity=excluded.identity, status=excluded.status,
                   started_at=excluded.started_at, ended_at=excluded.ended_at,
                   materials=excluded.materials, header_hash=excluded.header_hash,
                   updated_at=excluded.updated_at",
                params![project_id, meta.run_id, path, meta.role, meta.agent, meta.provider,
                    meta.model, meta.vendor, meta.visibility, meta.isolation, meta.identity,
                    meta.status, meta.started_at, meta.ended_at, serde_json::to_string(&meta.materials)?, hash, now],
            )?;
            outcome.files_updated += 1;
        }
        if complete {
            for path in existing.into_iter().filter(|p| {
                let owner = known
                    .iter()
                    .filter(|(id, root)| {
                        id == project_id && crate::is_under_dir(p, Path::new(root))
                    })
                    .max_by_key(|(_, root)| db::path_key(root).len())
                    .map(|(_, root)| db::path_key(root));
                owner == Some(db::path_key(root)) && !disk_paths.contains(p)
            }) {
                outcome.files_removed += tx.execute(
                    "DELETE FROM cross_checks WHERE project_id = ?1 AND path = ?2",
                    params![project_id, path],
                )?;
            }
        }
        tx.commit()?;
    }
    Ok(outcome)
}

pub fn for_project(conn: &Connection, project_id: i64) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT id, run_id, path, role, agent, provider, model, vendor, visibility,
                isolation, identity, status, started_at, ended_at, materials
         FROM cross_checks WHERE project_id = ?1
         ORDER BY julianday(COALESCE(started_at, ended_at)) DESC, run_id DESC, id",
    )?;
    let rows = stmt
        .query_map([project_id], |r| {
            let materials = r.get::<_, String>(14)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "run_id": r.get::<_, String>(1)?,
                "path": r.get::<_, String>(2)?,
                "role": r.get::<_, String>(3)?,
                "agent": r.get::<_, String>(4)?,
                "provider": r.get::<_, String>(5)?,
                "model": r.get::<_, String>(6)?,
                "vendor": r.get::<_, String>(7)?,
                "visibility": r.get::<_, String>(8)?,
                "isolation": r.get::<_, String>(9)?,
                "identity": r.get::<_, String>(10)?,
                "status": r.get::<_, String>(11)?,
                "started_at": r.get::<_, String>(12)?,
                "ended_at": r.get::<_, String>(13)?,
                "materials": serde_json::from_str::<Vec<String>>(&materials).unwrap_or_default(),
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut groups: Vec<Value> = Vec::new();
    let mut indices = HashMap::new();
    for row in rows {
        let run_id = row["run_id"].as_str().unwrap_or_default().to_string();
        let index = *indices.entry(run_id.clone()).or_insert_with(|| {
            groups.push(json!({ "run_id": run_id, "checks": [] }));
            groups.len() - 1
        });
        groups[index]["checks"]
            .as_array_mut()
            .expect("checks is an array")
            .push(row);
    }
    Ok(Value::Array(groups))
}

#[cfg(test)]
mod tests {
    use super::{parse_header, read_trace};

    #[test]
    fn parses_real_shaped_front_matter_without_reading_a_verdict() {
        let meta =
            parse_header(include_str!("../tests/fixtures/dispatch/blind-review.md")).unwrap();
        assert_eq!(meta.run_id, "run-blind-20260913");
        assert_eq!(meta.role, "风险审查");
        assert_eq!(meta.model, "model-luna");
        assert_eq!(meta.vendor, "Example AI");
        assert_eq!(meta.visibility, "盲");
        assert_eq!(meta.isolation, "部分");
        assert_eq!(meta.identity, "");
        assert_eq!(meta.status, "ok");
        assert_eq!(meta.materials, ["plan.md", "data.csv"]);
    }

    #[test]
    fn accepts_json_yaml_and_rejects_other_documents() {
        let header = "---\nschema: \"aicross-dispatch/1\"\nrun_id: \"run-1\"\nmaterials: [\"a.md\"]\nidentity: null\n---\n## 任务全文\nsecret";
        let meta = parse_header(header).unwrap();
        assert_eq!(meta.run_id, "run-1");
        assert_eq!(meta.materials, ["a.md"]);
        assert!(parse_header("---\nschema: other\nrun_id: run-1\n---\n").is_none());
        assert!(parse_header("---\nschema: aicross-dispatch/1\n---\n").is_none());
    }

    #[test]
    fn large_non_utf8_aligned_body_does_not_affect_header_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.md");
        let mut content = include_str!("../tests/fixtures/dispatch/blind-review.md").to_string();
        content.push_str(&"回答".repeat(40_000));
        std::fs::write(&path, content).unwrap();
        let (meta, _) = read_trace(&path).unwrap();
        assert_eq!(meta.run_id, "run-blind-20260913");
    }
}
