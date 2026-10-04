//! 项目文档跟踪：根目录发现（对话在子目录也覆盖根目录）、手动登记、
//! 写入对话对照与过时提示。

use std::time::{Duration, SystemTime};

use yourmem::{db, memfiles, project_docs};

fn insert_session(conn: &rusqlite::Connection, pid: i64, id: &str, cwd: &str, started: &str, messages: i64) {
    conn.execute(
        "INSERT INTO sessions(id,agent,native_id,project_id,file_path,cwd,started_at,ended_at,message_count,created_at,updated_at)
         VALUES (?1,'claude',?1,?2,'/tmp/x.jsonl',?3,?4,?4,?5,?4,?4)",
        rusqlite::params![id, pid, cwd, started, messages],
    )
    .unwrap();
}

#[test]
fn project_docs_are_discovered_tracked_linked_and_flagged_stale() {
    let home = tempfile::tempdir().unwrap();
    let agent_dirs = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let root = proj.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("CLAUDE.md"), "# rules\n").unwrap();
    std::fs::write(root.join("progress.md"), "## 下一步\n- 写测试\n").unwrap();
    std::fs::write(root.join("docs/plan.md"), "plan\n").unwrap();
    std::fs::write(root.join("notes.md"), "not a progress doc\n").unwrap();
    // 进度文档停在 2026-01-01，CLAUDE.md 更新于 2026-03-01（晚于下面所有对话）
    let set_mtime = |name: &str, secs: u64| {
        std::fs::File::options().write(true).open(root.join(name)).unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    };
    set_mtime("progress.md", 1_767_225_600);
    set_mtime("CLAUDE.md", 1_772_323_200);

    let conn = db::open(home.path()).unwrap();
    let root_str = root.to_string_lossy().to_string();
    let (pid, _) = db::add_project(&conn, &root_str).unwrap();
    let sub = root.join("sub").to_string_lossy().to_string();

    // 写过 progress.md 的对话（相对路径 artifact，按 cwd 解析并规整 ./）
    insert_session(&conn, pid, "claude:w", &root_str, "2025-12-31T10:00:00Z", 4);
    conn.execute(
        "INSERT INTO session_artifacts(session_id,project_id,path,tool,created_at) VALUES ('claude:w',?1,'./progress.md','Edit','2025-12-31T11:00:00Z')",
        [pid],
    ).unwrap();
    // 此后 5 段有效对话（在子目录里开，时间戳格式混用），外加不计数的空对话、子任务，
    // 以及字符串上"更晚"、换算后早于文档更新的带偏移时间戳
    for i in 1..=4 {
        insert_session(&conn, pid, &format!("claude:s{i}"), &sub, &format!("2026-02-0{i}T10:00:00.000Z"), 3);
    }
    insert_session(&conn, pid, "codex:s5", &sub, "2026-02-05T18:00:00+08:00", 3);
    insert_session(&conn, pid, "codex:early", &sub, "2026-01-01T07:00:00+08:00", 3);
    insert_session(&conn, pid, "claude:empty", &sub, "2026-02-06T10:00:00Z", 0);
    insert_session(&conn, pid, "claude:child", &sub, "2026-02-07T10:00:00Z", 3);
    conn.execute(
        "INSERT INTO session_links(parent_session_id,child_session_id,link_type,created_at) VALUES ('claude:s1','claude:child','subagent','2026-02-07T10:00:00Z')",
        [],
    ).unwrap();

    // 手动登记：越界路径拒绝，项目内文件接受，自动识别的文件不重复登记
    assert!(project_docs::track(&conn, home.path(), pid, "../x.md").is_err());
    project_docs::track(&conn, home.path(), pid, "docs/plan.md").unwrap();
    assert_eq!(project_docs::track(&conn, home.path(), pid, "PROGRESS.md").unwrap()["already_tracked"], true);
    // 手改 config.json 写入的越界路径在读取侧同样被忽略
    let mut cfg = yourmem::ingest::read_config(home.path());
    cfg["tracked_docs"].as_array_mut().unwrap()
        .push(serde_json::json!({ "project": root_str, "path": "../outside.md" }));
    yourmem::ingest::write_config(home.path(), &cfg).unwrap();
    std::fs::write(root.parent().unwrap().join("outside.md"), "x").ok();

    // 对话 cwd 在子目录，仍备份根目录的 CLAUDE.md、progress.md 与登记文件；notes.md 不在范围。
    // 项目目录里的文档不随 agent 停用而停止备份
    yourmem::ingest::set_agent_disabled(home.path(), "claude", true).unwrap();
    let dirs = memfiles::SourceDirs { claude: agent_dirs.path().join(".claude"), codex: agent_dirs.path().join(".codex") };
    memfiles::collect(&conn, home.path(), &dirs).unwrap();
    let files = db::list_memory_files(&conn).unwrap();
    let names: Vec<String> = files.iter().map(|f| {
        std::path::Path::new(f["path"].as_str().unwrap()).file_name().unwrap().to_string_lossy().to_string()
    }).collect();
    assert_eq!(files.len(), 3, "{names:?}");
    for n in ["CLAUDE.md", "progress.md", "plan.md"] {
        assert!(names.iter().any(|x| x == n), "{names:?}");
    }
    assert!(files.iter().any(|f| f["agent"] == "doc" && f["path"].as_str().unwrap().ends_with("progress.md")));

    let st = project_docs::status(&conn, home.path(), pid).unwrap();
    let docs = st["docs"].as_array().unwrap();
    let doc = |n: &str| docs.iter().find(|d| d["name"] == n).unwrap_or_else(|| panic!("{n}: {docs:?}"));

    let progress = doc("progress.md");
    assert_eq!(progress["sessions_since"], 5);
    assert_eq!(progress["stale"], true);
    assert_eq!(progress["edited_by_total"], 1);
    assert_eq!(progress["edited_by"][0]["session_id"], "claude:w");
    assert!(progress["file_id"].is_i64());

    let claude_md = doc("CLAUDE.md");
    assert_eq!(claude_md["sessions_since"], 0);
    assert_eq!(claude_md["stale"], false);
    assert_eq!(claude_md["agent"], "claude");

    assert_eq!(doc("docs/plan.md")["manual"], true);
    assert_eq!(docs.len(), 3, "{docs:?}");

    project_docs::untrack(&conn, home.path(), pid, "docs/plan.md").unwrap();
    assert!(project_docs::untrack(&conn, home.path(), pid, "docs/plan.md").is_err());
    let st = project_docs::status(&conn, home.path(), pid).unwrap();
    assert!(st["docs"].as_array().unwrap().iter().all(|d| d["name"] != "docs/plan.md"));
}

/// 项目内的符号链接若指向项目目录之外，不登记、不采集。
#[test]
fn symlinked_docs_outside_the_project_are_not_collected() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
    let link = proj.path().join("progress.md");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(outside.path().join("secret.md"), &link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(outside.path().join("secret.md"), &link);
    if made.is_err() {
        eprintln!("skip: 当前环境不允许创建符号链接");
        return;
    }
    let conn = db::open(home.path()).unwrap();
    let root_str = proj.path().to_string_lossy().to_string();
    let (pid, _) = db::add_project(&conn, &root_str).unwrap();
    assert!(project_docs::track(&conn, home.path(), pid, "progress.md").is_err());
    let st = project_docs::status(&conn, home.path(), pid).unwrap();
    assert!(st["docs"].as_array().unwrap().is_empty(), "{st}");
}

/// 手动添加、尚无对话的项目，根目录文档同样备份。
#[test]
fn projects_without_sessions_still_back_up_their_docs() {
    let home = tempfile::tempdir().unwrap();
    let agent_dirs = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("HANDOFF.md"), "next\n").unwrap();
    let conn = db::open(home.path()).unwrap();
    let (pid, _) = db::add_project(&conn, &proj.path().to_string_lossy()).unwrap();
    let dirs = memfiles::SourceDirs { claude: agent_dirs.path().join(".claude"), codex: agent_dirs.path().join(".codex") };
    memfiles::collect(&conn, home.path(), &dirs).unwrap();
    let st = project_docs::status(&conn, home.path(), pid).unwrap();
    assert!(st["docs"][0]["file_id"].is_i64(), "{st}");
}
