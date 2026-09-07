use serde_json::{json, Value};
use std::path::Path;
use yourmem::{adapters, bundle, db, ingest, snapshots, vault};

fn append(home: &Path, source: &Path, number: u32) {
    use std::io::Write;
    let line = json!({"type":"user","uuid":format!("s{number}"),"cwd":"/tmp/snapshot-demo",
        "timestamp":"2026-09-07T00:00:00Z","message":{"role":"user","content":format!("快照检索第{number}条 {}","正文内容".repeat(500))}});
    writeln!(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(source.join("demo.jsonl"))
            .unwrap(),
        "{line}"
    )
    .unwrap();
    let mut conn = db::open(home).unwrap();
    ingest::import_all(
        &mut conn,
        home,
        &[(adapters::AGENT_CLAUDE, source.to_path_buf())],
        None,
        None,
    )
    .unwrap();
}
fn id(v: &Value) -> &str {
    v["id"].as_str().unwrap()
}

#[test]
fn shared_snapshots_export_restore_and_cleanup_keep_referenced_objects() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    append(home.path(), source.path(), 1);
    let first = snapshots::create(home.path()).unwrap();
    let second = snapshots::create(home.path()).unwrap();
    assert_eq!(second["new_objects"], 0);
    assert!(second["new_bytes"].as_u64().unwrap() < first["new_bytes"].as_u64().unwrap());
    append(home.path(), source.path(), 2);
    let third = snapshots::create(home.path()).unwrap();
    assert_eq!(third["new_objects"], 1);
    let conn = db::open(home.path()).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM messages_fts_docsize", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    // Remove live raw files: snapshot export must use its independent repository.
    let hashes: Vec<String> = conn
        .prepare("SELECT hash FROM vault_lines")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for hash in &hashes {
        std::fs::remove_file(vault::object_path(home.path(), hash)).unwrap();
    }
    let plan = snapshots::cleanup_plan(home.path(), 1, 0).unwrap();
    assert_eq!(plan["remove_count"], 2);
    snapshots::cleanup(home.path(), 1, 0, plan["token"].as_str().unwrap()).unwrap();
    assert_eq!(
        snapshots::list(home.path()).unwrap()["snapshots"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let out = home.path().join("complete.tar.gz");
    snapshots::export(home.path(), id(&third), &out).unwrap();
    assert_eq!(bundle::verify(&out).unwrap()["ok"], true);
    let restored = tempfile::tempdir().unwrap();
    let fresh_path = restored.path().join("new-library");
    bundle::restore(&out, &fresh_path, false).unwrap();
    let restored_conn = db::open(&fresh_path).unwrap();
    assert_eq!(
        restored_conn
            .query_row("SELECT count(*) FROM messages_fts_docsize", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let hits: i64 = restored_conn
        .query_row(
            "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH '快照检索'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 2);
    assert!(snapshots::export(home.path(), id(&first), &out).is_err());
}

#[test]
fn cleanup_requires_current_plan_and_stops_on_damaged_manifest() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    append(home.path(), source.path(), 1);
    snapshots::create(home.path()).unwrap();
    let plan = snapshots::cleanup_plan(home.path(), 1, 0).unwrap();
    let later = snapshots::create(home.path()).unwrap();
    assert!(snapshots::cleanup(home.path(), 1, 0, plan["token"].as_str().unwrap()).is_err());
    let manifest = snapshots::root(home.path())
        .join("snapshots")
        .join(format!("{}.json", id(&later)));
    let mut data: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    data["hashes"] = json!([]);
    std::fs::write(&manifest, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(snapshots::cleanup_plan(home.path(), 1, 0).is_err());
    assert_eq!(
        std::fs::read_dir(snapshots::root(home.path()).join("snapshots"))
            .unwrap()
            .count(),
        2
    );
    assert!(snapshots::export(home.path(), "../bad", &home.path().join("bad.tar.gz")).is_err());
}

#[test]
fn corrupt_source_cannot_publish_snapshot_and_orphans_can_be_previewed() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    append(home.path(), source.path(), 1);
    let conn = db::open(home.path()).unwrap();
    let hash: String = conn
        .query_row("SELECT hash FROM vault_lines LIMIT 1", [], |r| r.get(0))
        .unwrap();
    std::fs::write(vault::object_path(home.path(), &hash), b"damaged").unwrap();
    assert!(snapshots::create(home.path()).is_err());
    assert!(snapshots::list(home.path()).unwrap()["snapshots"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(snapshots::cleanup_plan(home.path(), 0, 0).is_err());
}

#[test]
fn retention_unions_recent_snapshots_with_latest_in_each_month() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    append(home.path(), source.path(), 1);
    let mut ids = Vec::new();
    for date in [
        "2026-01-01",
        "2026-01-20",
        "2026-02-01",
        "2026-03-01",
        "2026-03-20",
    ] {
        let s = snapshots::create(home.path()).unwrap();
        let path = snapshots::root(home.path())
            .join("snapshots")
            .join(format!("{}.json", id(&s)));
        let mut m: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        m["created_at"] = json!(format!("{date}T00:00:00Z"));
        std::fs::write(path, serde_json::to_vec(&m).unwrap()).unwrap();
        ids.push(id(&s).to_string());
    }
    let p = snapshots::cleanup_plan(home.path(), 1, 3).unwrap();
    assert_eq!(p["remove_count"], 2);
    assert!(p["remove_ids"].as_array().unwrap().contains(&json!(ids[0])));
    assert!(p["remove_ids"].as_array().unwrap().contains(&json!(ids[3])));
}
