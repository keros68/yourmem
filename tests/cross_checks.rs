use std::path::Path;

use yourmem::{cross_checks, db, dossier};

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn copy_fixtures(dispatch: &Path) {
    std::fs::create_dir_all(dispatch.join("runtime")).unwrap();
    for name in ["blind-review.md", "blind-second.md", "shared-execution.md"] {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dispatch").join(name);
        std::fs::copy(source, dispatch.join(name)).unwrap();
    }
    std::fs::write(dispatch.join("ignored.raw.txt"), "not a trace").unwrap();
    std::fs::write(
        dispatch.join("runtime/ignored.md"),
        "---\nschema: aicross-dispatch/1\nrun_id: runtime\n---\n",
    ).unwrap();
    std::fs::write(dispatch.join("ordinary.md"), "# notes\n").unwrap();
}

#[test]
fn collects_groups_updates_and_removes_only_readable_trace_metadata() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let project = tempfile::tempdir().unwrap();
    let (pid, _) = db::add_project(&conn, text(project.path())).unwrap();
    let dispatch = project.path().join(".dispatch");
    copy_fixtures(&dispatch);
    let original = std::fs::read(dispatch.join("blind-review.md")).unwrap();

    let first = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!((first.files_seen, first.files_updated, first.files_removed), (3, 3, 0));
    assert_eq!(std::fs::read(dispatch.join("blind-review.md")).unwrap(), original);
    assert_eq!(db::SCHEMA_VERSION, 18);

    let second = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!((second.files_seen, second.files_updated, second.files_removed), (3, 0, 0));

    let d = dossier::project_dossier(&conn, pid).unwrap();
    let runs = d["cross_checks"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    let blind = runs.iter().find(|r| r["run_id"] == "run-blind-20260913").unwrap();
    assert_eq!(blind["checks"].as_array().unwrap().len(), 2);
    let review = blind["checks"].as_array().unwrap().iter()
        .find(|c| c["role"] == "风险审查").unwrap();
    assert_eq!(review["vendor"], "Example AI");
    assert_eq!(review["model"], "model-luna");
    assert_eq!(review["visibility"], "盲");
    assert_eq!(review["isolation"], "部分");
    assert_eq!(review["identity"], "");
    assert_eq!(review["status"], "ok");
    assert_eq!(review["materials"], serde_json::json!(["plan.md", "data.csv"]));
    assert!(review["path"].as_str().unwrap().ends_with("blind-review.md"));
    assert!(!d.to_string().contains("## 原始输出"));
    let md = dossier::render_markdown(&d);
    assert!(md.contains("## 交叉核对"));
    assert!(md.contains("run-blind-20260913"));
    assert!(md.contains("Example AI / model-luna"));

    // 正文变化不影响元数据；YAML 头变化才更新。
    let trace = dispatch.join("blind-review.md");
    let header_only = std::fs::read_to_string(&trace).unwrap();
    std::fs::write(&trace, header_only.replace("## 原始输出\n", "## 原始输出\n\n未入库的回答正文\n")).unwrap();
    let body_change = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!(body_change.files_updated, 0);
    let changed = std::fs::read_to_string(&trace).unwrap().replace("status: ok", "status: failed");
    std::fs::write(&trace, changed).unwrap();
    let header_change = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!(header_change.files_updated, 1);
    assert!(cross_checks::for_project(&conn, pid).unwrap().to_string().contains("failed"));
    assert!(!cross_checks::for_project(&conn, pid).unwrap().to_string().contains("未入库的回答正文"));

    std::fs::remove_file(&trace).unwrap();
    let removed = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!(removed.files_removed, 1);
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM cross_checks WHERE project_id = ?1", [pid], |r| r.get(0)).unwrap();
    assert_eq!(count, 2);
}

#[test]
fn missing_project_directory_is_skipped_without_dropping_history() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let project = tempfile::tempdir().unwrap();
    let (pid, _) = db::add_project(&conn, text(project.path())).unwrap();
    let dispatch = project.path().join(".dispatch");
    copy_fixtures(&dispatch);
    cross_checks::collect(&conn, home.path()).unwrap();

    std::fs::remove_dir_all(project.path()).unwrap();
    let outcome = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!((outcome.files_seen, outcome.files_updated, outcome.files_removed), (0, 0, 0));
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM cross_checks WHERE project_id = ?1", [pid], |r| r.get(0)).unwrap();
    assert_eq!(count, 3);
}

#[test]
fn merged_project_keeps_collecting_traces_at_their_original_path() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let base = tempfile::tempdir().unwrap();
    let parent = base.path().join("research");
    let child = parent.join("current");
    std::fs::create_dir_all(&child).unwrap();
    let (parent_id, _) = db::add_project(&conn, text(&parent)).unwrap();
    let (child_id, _) = db::add_project(&conn, text(&child)).unwrap();
    let dispatch = child.join(".dispatch");
    std::fs::create_dir_all(&dispatch).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dispatch/blind-review.md"),
        dispatch.join("review.md"),
    ).unwrap();
    cross_checks::collect(&conn, home.path()).unwrap();
    db::merge_project(&conn, child_id, parent_id).unwrap();

    let unchanged = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!((unchanged.files_seen, unchanged.files_updated, unchanged.files_removed), (1, 0, 0));
    let trace = dispatch.join("review.md");
    let changed = std::fs::read_to_string(&trace).unwrap().replace("status: ok", "status: failed");
    std::fs::write(&trace, changed).unwrap();
    let updated = cross_checks::collect(&conn, home.path()).unwrap();
    assert_eq!(updated.files_updated, 1);
    let status: String = conn.query_row(
        "SELECT status FROM cross_checks WHERE project_id = ?1", [parent_id], |r| r.get(0),
    ).unwrap();
    assert_eq!(status, "failed");
}
