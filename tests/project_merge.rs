use std::path::Path;

use yourmem::db;

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn non_git_cwd_uses_nearest_active_project_and_its_context() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    // 用虚构的非临时目录：临时目录属运行现场，不参与上级归属
    let base = std::path::PathBuf::from("/work/pm-non-git");
    let root = base.join("research");
    let nested = root.join("drafts");
    let cwd = nested.join("current").join("figures");
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
    let base = std::path::PathBuf::from("/work/pm-win-case");
    let parent = base.join("Research");
    let cwd = parent.join("Drafts").join("Current");
    let (pid, _) = db::add_project(&conn, text(&parent)).unwrap();
    let variant = text(&cwd).replace('\\', "/").to_uppercase();

    assert_eq!(db::project_root_for_conn(&conn, &variant).unwrap().0, text(&parent));
    assert_eq!(db::resolve_project(&conn, None, Some(&variant)).unwrap().unwrap().0, pid);
}

#[test]
fn merge_previews_and_moves_all_assets_then_archives_without_revival() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let base = std::path::PathBuf::from("/work/pm-merge");
    let parent = base.join("research");
    let child = parent.join("current");
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

#[test]
fn archived_scratch_cwd_never_falls_back_to_most_active_project() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    // 最近活跃的普通项目：旧的"最近活跃"兜底会把它返回给任何未命中的 cwd
    let active_id = db::upsert_project(&conn, "/work/reviewed-project", "reviewed-project").unwrap();
    let scratch = yourmem::home_dir()
        .join(".aicross")
        .join("scratch")
        .join("20261024-101010-blind")
        .join("codex.openai");
    let scratch = text(&scratch);
    let scratch_id = db::upsert_project(&conn, scratch, "codex.openai").unwrap();
    let archived: Option<String> = conn
        .query_row("SELECT archived_at FROM projects WHERE id = ?1", [scratch_id], |r| r.get(0))
        .unwrap();
    assert!(archived.is_some(), "盲审目录新建即归档");

    for cwd in [scratch.to_string(), Path::new(scratch).join("notes").to_string_lossy().to_string()] {
        let hit = db::resolve_project(&conn, None, Some(&cwd)).unwrap().unwrap();
        assert_eq!(hit.0, scratch_id, "{cwd} 应命中已归档项目本身");
        assert_ne!(hit.0, active_id, "{cwd} 不得落到最近活跃项目");
    }
    // 未登记为项目的盲审目录：返回 None，同样不落兜底
    let unregistered = yourmem::home_dir()
        .join(".aicross")
        .join("scratch")
        .join("20261016-103711-blind")
        .join("agy.google");
    assert!(db::resolve_project(&conn, None, Some(text(&unregistered))).unwrap().is_none());
}

#[test]
fn dispatch_dirs_under_non_git_project_stay_independent_archived_projects() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let parent_id = db::upsert_project(&conn, "/work/research-non-git", "research-non-git").unwrap();
    for cwd in [
        "/work/research-non-git/.dispatch/runtime/runs/x",
        "/work/research-non-git/.dispatch/20261024-blind",
    ] {
        let (root, _) = db::project_root_for_conn(&conn, cwd).unwrap();
        assert_eq!(root, cwd, "运行现场不做上级归属");
        let id = db::upsert_project(&conn, &root, "run").unwrap();
        assert_ne!(id, parent_id);
        let archived: Option<String> = conn
            .query_row("SELECT archived_at FROM projects WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert!(archived.is_some(), "{cwd} 仍新建为已归档项目");
    }
    let parent_archived: Option<String> = conn
        .query_row("SELECT archived_at FROM projects WHERE id = ?1", [parent_id], |r| r.get(0))
        .unwrap();
    assert!(parent_archived.is_none(), "所在项目本身保持活跃");
}
