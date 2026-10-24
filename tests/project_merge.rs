use std::path::Path;

use yourmem::db;

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn non_git_cwd_uses_nearest_active_project_and_its_context() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("research");
    let nested = root.join("drafts");
    let cwd = nested.join("current").join("figures");
    std::fs::create_dir_all(&cwd).unwrap();
    let (root_id, _) = db::add_project(&conn, text(&root)).unwrap();
    let (nested_id, _) = db::add_project(&conn, text(&nested)).unwrap();

    let (path, name) = db::project_root_for_conn(&conn, text(&cwd)).unwrap();
    assert_eq!((path, name), (text(&nested).to_string(), "drafts".to_string()));
    assert_eq!(db::resolve_project(&conn, None, Some(text(&cwd))).unwrap().unwrap().0, nested_id);

    db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(root_id), scope: "project", r#type: "fact",
        content: "根项目的研究方法", status: None,
        source_session_id: None, source_message_id: None,
    }).unwrap();
    db::set_project_archived(&conn, nested_id, true).unwrap();
    let resolved = db::resolve_project(&conn, None, Some(text(&cwd))).unwrap().unwrap();
    assert_eq!(resolved.0, root_id);
    let context = db::project_context(&conn, resolved.0).unwrap();
    assert!(context["memories"]["confirmed"].as_array().unwrap().iter().any(|m| m["content"] == "根项目的研究方法"));
    assert_eq!(db::resolve_project(&conn, Some(text(&nested)), None).unwrap().unwrap().0, nested_id);
}

#[test]
fn git_root_takes_priority_over_registered_parent() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let base = tempfile::tempdir().unwrap();
    let parent = base.path().join("parent");
    let repo = parent.join("repo");
    let cwd = repo.join("src");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    db::add_project(&conn, text(&parent)).unwrap();

    let (root, name) = db::project_root_for_conn(&conn, text(&cwd)).unwrap();
    assert_eq!((root, name), (text(&repo).to_string(), "repo".to_string()));
}

#[test]
fn home_temp_and_filesystem_root_cannot_capture_descendants() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let user_home = yourmem::home_dir();
    db::add_project(&conn, text(&user_home)).unwrap();
    let under_home = user_home.join("Documents").join("Codex").join("2026-10-24").join("example");
    assert_eq!(db::project_root_for_conn(&conn, text(&under_home)).unwrap().0, text(&under_home));

    let other_home = tempfile::tempdir().unwrap();
    let other = db::open(other_home.path()).unwrap();
    let temp = std::env::temp_dir();
    db::add_project(&other, text(&temp)).unwrap();
    let under_temp = temp.join("yourmem-parent-exclusion").join("child");
    assert_eq!(db::project_root_for_conn(&other, text(&under_temp)).unwrap().0, text(&under_temp));

    let third_home = tempfile::tempdir().unwrap();
    let third = db::open(third_home.path()).unwrap();
    let fs_root = third_home.path().ancestors().last().unwrap();
    db::upsert_project(&third, text(fs_root), "root").unwrap();
    let under_root = fs_root.join("yourmem-root-exclusion").join("child");
    assert_eq!(db::project_root_for_conn(&third, text(&under_root)).unwrap().0, text(&under_root));
}

#[cfg(windows)]
#[test]
fn windows_ancestor_matching_ignores_case_and_separators() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let base = tempfile::tempdir().unwrap();
    let parent = base.path().join("Research");
    let cwd = parent.join("Drafts").join("Current");
    std::fs::create_dir_all(&cwd).unwrap();
    let (pid, _) = db::add_project(&conn, text(&parent)).unwrap();
    let variant = text(&cwd).replace('\\', "/").to_uppercase();

    assert_eq!(db::project_root_for_conn(&conn, &variant).unwrap().0, text(&parent));
    assert_eq!(db::resolve_project(&conn, None, Some(&variant)).unwrap().unwrap().0, pid);
}

#[test]
fn merge_previews_and_moves_all_assets_then_archives_without_revival() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let base = tempfile::tempdir().unwrap();
    let parent = base.path().join("research");
    let child = parent.join("current");
    std::fs::create_dir_all(&child).unwrap();
    let (parent_id, _) = db::add_project(&conn, text(&parent)).unwrap();
    let (child_id, _) = db::add_project(&conn, text(&child)).unwrap();
    conn.execute(
        "INSERT INTO sessions(id, agent, native_id, project_id, file_path, cwd, created_at, updated_at)
         VALUES ('claude:merge-test', 'claude', 'merge-test', ?1, 'fixture', ?2, '2026-10-24', '2026-10-24')",
        rusqlite::params![child_id, text(&child)],
    ).unwrap();
    db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(child_id), scope: "project", r#type: "decision",
        content: "保留当前稿作为主线", status: None,
        source_session_id: None, source_message_id: None,
    }).unwrap();
    db::create_handoff(&conn, child_id, &db::HandoffFields::default()).unwrap();
    conn.execute(
        "INSERT INTO session_artifacts(session_id, project_id, path, tool, created_at)
         VALUES ('claude:merge-test', ?1, 'draft.md', 'Write', '2026-10-24')",
        [child_id],
    ).unwrap();
    conn.execute(
        "INSERT INTO cross_checks(project_id, run_id, path, role, model, vendor, header_hash, created_at, updated_at)
         VALUES (?1, 'run-merge', '.dispatch/review.md', '盲审', 'model', 'vendor', 'hash', '2026-10-24', '2026-10-24')",
        [child_id],
    ).unwrap();

    let plan = db::project_merge_plan(&conn, child_id, None).unwrap();
    assert_eq!(plan["parent"]["id"], parent_id);
    assert_eq!(plan["sessions"], 1);
    assert_eq!(plan["memories"], 1);
    assert_eq!(plan["artifacts"], 1);
    assert_eq!(plan["handoffs"], 1);
    assert_eq!(plan["cross_checks"], 1);
    assert_eq!(plan["targets"].as_array().unwrap().len(), 1);

    let merged = db::merge_project(&conn, child_id, parent_id).unwrap();
    assert_eq!(merged["merged"], true);
    for table in ["sessions", "memories", "session_artifacts", "handoffs", "cross_checks"] {
        let at_parent: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?1"),
            [parent_id],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(at_parent, 1, "{table} must move to the parent");
    }
    let archived: Option<String> = conn.query_row(
        "SELECT archived_at FROM projects WHERE id = ?1", [child_id], |r| r.get(0),
    ).unwrap();
    assert!(archived.is_some());
    assert!(db::project_merge_plan(&conn, child_id, None).is_err());

    db::upsert_project(&conn, text(&child), "current").unwrap();
    let still_archived: Option<String> = conn.query_row(
        "SELECT archived_at FROM projects WHERE id = ?1", [child_id], |r| r.get(0),
    ).unwrap();
    assert!(still_archived.is_some());
    assert_eq!(db::project_root_for_conn(&conn, text(&child)).unwrap().0, text(&parent));
    assert_eq!(db::resolve_project(&conn, None, Some(text(&child))).unwrap().unwrap().0, parent_id);
}
