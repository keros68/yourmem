use rusqlite::{params, Connection};
use yourmem::{db, dossier, models::SessionMetaPatch, project_review, recall_status};

fn session(c: &Connection, id: &str, pid: Option<i64>) {
    db::upsert_session(
        c,
        id,
        "claude",
        id,
        pid,
        "fixture",
        &SessionMetaPatch::default(),
        0,
    )
    .unwrap();
}
fn memory(c: &Connection, content: &str) -> String {
    db::save_memory(
        c,
        &db::MemoryInput {
            project_id: None,
            scope: "global",
            r#type: "lesson",
            content,
            status: None,
            source_session_id: None,
            source_message_id: None,
        },
    )
    .unwrap()
}

#[test]
fn focused_windows_page_both_ways_and_retrieve_full_text() {
    let home = tempfile::tempdir().unwrap();
    let c = db::open(home.path()).unwrap();
    session(&c, "s", None);
    session(&c, "other", None);
    for n in 0..400 {
        c.execute(
            "INSERT INTO messages(session_id,line_no,ord,kind,content) VALUES('s',?1,?2,'user',?3)",
            params![
                n / 2,
                n % 2,
                if n == 200 {
                    "命中".repeat(2500)
                } else {
                    format!("message {n}")
                }
            ],
        )
        .unwrap();
    }
    let hits = db::search(
        &c,
        &db::SearchOpts {
            query: "命中".into(),
            project: None,
            agent: None,
            kind: None,
            limit: 10,
        },
    )
    .unwrap();
    let mid = hits[0]["message_id"].as_i64().unwrap();
    assert_eq!(hits[0]["line_no"], 100);
    let w = db::session_window(&c, "s", Some(100), None, Some(mid), false).unwrap();
    assert_eq!(w["window_offset"], 125);
    assert_eq!(w["messages"][75]["message_id"], mid);
    assert_eq!(w["messages"][75]["truncated"], true);
    assert_eq!(
        db::message_content(&c, "s", mid).unwrap()["content"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        5000
    );
    assert!(db::message_content(&c, "other", mid).is_err());
    assert!(db::session_window(&c, "other", None, None, Some(mid), false).is_err());
    let mut ids = Vec::new();
    for offset in [0, 150, 300] {
        let page = db::session_window(&c, "s", None, Some(offset), None, false).unwrap();
        ids.extend(
            page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["message_id"].as_i64().unwrap()),
        );
        assert_eq!(page["has_after"], offset < 300);
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 400);
    c.execute("UPDATE sessions SET compact_line_no=100 WHERE id='s'", [])
        .unwrap();
    let before = db::session_window(&c, "s", None, Some(150), None, true).unwrap();
    assert_eq!(before["total_messages"], 200);
    assert_eq!(before["messages"].as_array().unwrap().len(), 50);
    assert_eq!(before["has_after"], false);
    db::set_session_deleted(&c, "s", true).unwrap();
    assert!(db::session_window(&c, "s", None, None, None, false).is_err());
    assert!(db::message_content(&c, "s", mid).is_err());
}

#[test]
fn exact_message_focus_handles_many_blocks_on_one_source_line() {
    let home = tempfile::tempdir().unwrap();
    let c = db::open(home.path()).unwrap();
    session(&c, "s", None);
    for ord in 0..350 {
        c.execute("INSERT INTO messages(session_id,line_no,ord,kind,content) VALUES('s',1,?1,'assistant','block')", [ord]).unwrap();
    }
    let mid = c.last_insert_rowid();
    let w = db::session_window(&c, "s", Some(1), None, Some(mid), false).unwrap();
    assert!(w["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["message_id"] == mid));
}

#[test]
fn replacement_validation_is_atomic_and_accepts_convergence() {
    let home = tempfile::tempdir().unwrap();
    let c = db::open(home.path()).unwrap();
    let (a, b, d) = (memory(&c, "A"), memory(&c, "B"), memory(&c, "D"));
    for target in [&a, "missing"] {
        assert!(db::update_memory_status(&c, &a, "supersede", Some(target)).is_err());
        assert_eq!(
            c.query_row("SELECT status FROM memories WHERE id=?1", [&a], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "confirmed"
        );
    }
    db::update_memory_status(&c, &a, "supersede", Some(&b)).unwrap();
    db::update_memory_status(&c, &d, "supersede", Some(&b)).unwrap();
    assert!(db::update_memory_status(&c, &b, "supersede", Some(&a)).is_err());
    db::update_memory_status(&c, &a, "confirm", None).unwrap();
    db::update_memory_status(&c, &b, "supersede", Some(&a)).unwrap();
}

#[test]
fn lineage_cap_is_strict_and_dossier_keeps_multiple_parents() {
    let home = tempfile::tempdir().unwrap();
    let c = db::open(home.path()).unwrap();
    let pid = db::upsert_project(&c, "fixture", "fixture").unwrap();
    session(&c, "root", Some(pid));
    session(&c, "second", Some(pid));
    for n in 0..305 {
        let child = format!("child-{n:03}");
        session(&c, &child, Some(pid));
        c.execute("INSERT INTO session_links(parent_session_id,child_session_id,link_type,created_at) VALUES('root',?1,'fork','2026-09-06')", [&child]).unwrap();
    }
    c.execute("INSERT INTO session_links(parent_session_id,child_session_id,link_type,created_at) VALUES('second','child-000','continuation','2026-09-06')", []).unwrap();
    let graph = db::lineage_tree(&c, "root").unwrap();
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 300);
    assert_eq!(graph["truncated"], true);
    let ids: Vec<_> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["session_id"].as_str().unwrap())
        .collect();
    assert!(graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| ids.contains(&e["parent"].as_str().unwrap())
            && ids.contains(&e["child"].as_str().unwrap())));
    assert_eq!(graph, db::lineage_tree(&c, "root").unwrap());
    let d = dossier::project_dossier(&c, pid).unwrap();
    assert_eq!(
        d["lineage_edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["c"] == "child-000")
            .count(),
        2
    );
}

#[test]
fn recall_status_distinguishes_unknown_empty_success_and_failure() {
    let home = tempfile::tempdir().unwrap();
    let row = || {
        recall_status::read(home.path())
            .into_iter()
            .find(|r| r["name"] == "search_history")
            .unwrap()
    };
    assert_eq!(row()["ok"], serde_json::Value::Null);
    recall_status::record(home.path(), "mcp", "search_history", true, Some(0));
    let success = row();
    assert_eq!(success["ok"], true);
    assert_eq!(success["result_count"], 0);
    recall_status::record(home.path(), "mcp", "search_history", false, None);
    assert_eq!(row()["ok"], false);
    assert_eq!(row()["last_success"], success["last_success"]);
    recall_status::record(home.path(), "mcp", "../query-secret", true, None);
    assert_eq!(
        std::fs::read_dir(home.path().join("recall-status"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn git_baseline_requires_explicit_review_and_does_not_change_memory_status() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("source.txt"), "first").unwrap();
    git(&["add", "source.txt"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "first",
    ]);
    let c = db::open(home.path()).unwrap();
    let pid = db::upsert_project(&c, repo.path().to_str().unwrap(), "test").unwrap();
    let id = memory(&c, "verified lesson");
    assert_eq!(
        project_review::status(&c, home.path(), pid).unwrap()["status"],
        "unreviewed"
    );
    assert_eq!(
        project_review::mark_reviewed(&c, home.path(), pid).unwrap()["status"],
        "unchanged"
    );
    std::fs::write(repo.path().join("source.txt"), "changed").unwrap();
    assert_eq!(
        project_review::status(&c, home.path(), pid).unwrap()["status"],
        "changed"
    );
    assert!(project_review::mark_reviewed(&c, home.path(), pid).is_err());
    git(&["add", "source.txt"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "next",
    ]);
    assert_eq!(
        project_review::status(&c, home.path(), pid).unwrap()["status"],
        "changed"
    );
    assert_eq!(
        project_review::mark_reviewed(&c, home.path(), pid).unwrap()["status"],
        "unchanged"
    );
    assert_eq!(
        c.query_row("SELECT status FROM memories WHERE id=?1", [&id], |r| r
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "confirmed"
    );
    let missing =
        db::upsert_project(&c, home.path().join("absent").to_str().unwrap(), "absent").unwrap();
    assert_eq!(
        project_review::status(&c, home.path(), missing).unwrap()["status"],
        "unavailable"
    );
}
