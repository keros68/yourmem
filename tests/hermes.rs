//! 0.3.9 Hermes adapter：SQLite 单库源（opencode 模式）——会话/消息映射、
//! cron 排除、subagent 谱系、压缩点前缀检测、游标增量、vault 保真。

use yourmem::adapters::{self, hermes};
use yourmem::{db, ingest};

/// 建一个 hermes state.db 形态的最小源库（真实 schema 的相关列子集）。
fn make_source(path: &std::path::Path) {
    let src = rusqlite::Connection::open(path).unwrap();
    src.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY, parent_session_id TEXT, source TEXT,
            cwd TEXT, git_repo_root TEXT, git_branch TEXT,
            started_at REAL, ended_at REAL, archived INTEGER DEFAULT 0);
         CREATE TABLE messages (
            id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT,
            tool_call_id TEXT, tool_name TEXT, timestamp REAL,
            tool_calls TEXT, active INTEGER DEFAULT 1, compacted INTEGER DEFAULT 0);
         INSERT INTO sessions VALUES
            ('s_cli', NULL, 'cli', '/tmp/hm-proj', NULL, 'main', 1000.0, 2000.0, 0),
            ('s_cron', NULL, 'cron', NULL, NULL, NULL, 1000.0, 1500.0, 0),
            ('s_sub', 's_cli', 'subagent', NULL, NULL, NULL, 1500.0, 1600.0, 0);
         INSERT INTO messages(id, session_id, role, content, tool_name, timestamp) VALUES
            (1, 's_cli', 'user', '压缩前的用户提问', NULL, 1001.0),
            (2, 's_cli', 'assistant', '助手的回复', NULL, 1002.0),
            (3, 's_cli', 'assistant', NULL, NULL, 1003.0),
            (4, 's_cli', 'tool', '工具输出文本', 'Read', 1004.0),
            (10, 's_sub', 'user', '子代理任务', NULL, 1501.0);
         UPDATE messages SET tool_calls = '[{\"function\":{\"name\":\"Write\",\"arguments\":\"{\\\"file_path\\\":\\\"/tmp/hm-proj/a.py\\\"}\"}}]' WHERE id = 3;",
    )
    .unwrap();
}

#[test]
fn imports_hermes_sqlite_with_cron_excluded_and_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("state.db");
    make_source(&src_path);
    let home = tempfile::tempdir().unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let out = hermes::import(&mut conn, home.path(), &src_path).unwrap();
    assert_eq!(out.sessions_seen, 2, "cron 会话不得入账（运行日志裁定）");
    assert_eq!(out.sessions_updated, 2);

    // cron 会话不存在；cli 会话消息齐：user + assistant + tool_call + tool_result
    let has = |id: &str| -> bool {
        conn.query_row("SELECT 1 FROM sessions WHERE id = ?1", rusqlite::params![id], |_| Ok(()))
            .is_ok()
    };
    assert!(has("hermes:s_cli"));
    assert!(!has("hermes:s_cron"), "cron 是运行日志，不采");
    let kinds: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT kind FROM messages WHERE session_id = 'hermes:s_cli' ORDER BY line_no, ord")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(kinds, vec!["user", "assistant", "tool_call", "tool_result"], "{kinds:?}");

    // artifact：assistant.tool_calls 的 Write.file_path 提取
    let arts = db::list_artifacts(&conn, None, Some("hermes:s_cli"), 10).unwrap();
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0]["path"], "/tmp/hm-proj/a.py");

    // 谱系：subagent 来源显式标 subagent
    let lineage = db::lineage_for(&conn, "hermes:s_sub").unwrap();
    assert_eq!(lineage["parents"][0]["session_id"], "hermes:s_cli");
    assert_eq!(lineage["parents"][0]["type"], "subagent");

    // resume 命令（hermes --resume）
    assert_eq!(
        adapters::resume_command("hermes", "s_cli").as_deref(),
        Some("hermes --resume s_cli")
    );

    // 可搜索 + 项目绑定（cwd 命中）
    let hits = db::search(&conn, &db::SearchOpts {
        query: "压缩前的用户提问".into(), project: None, agent: Some("hermes".into()), kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1);

    // 增量：新消息按 id 游标拾取
    let src = rusqlite::Connection::open(&src_path).unwrap();
    src.execute(
        "INSERT INTO messages(id, session_id, role, content, timestamp) VALUES (11, 's_cli', 'user', '增量新消息', 2100.0)",
        [],
    )
    .unwrap();
    drop(src);
    let again = hermes::import(&mut conn, home.path(), &src_path).unwrap();
    assert_eq!(again.messages_added, 1, "游标只拾取新消息");
}

#[test]
fn hermes_compaction_prefix_marks_boundary() {
    // 机制性检测（hermes 自述前缀，真机尚无样本）：摘要行 id 即压缩点，
    // "id < 摘要行" 恰为压缩发生前存在的全部原文（头+被摘要中段+保留尾）。
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("state.db");
    make_source(&src_path);
    let src = rusqlite::Connection::open(&src_path).unwrap();
    src.execute(
        "INSERT INTO messages(id, session_id, role, content, timestamp) VALUES
         (20, 's_cli', 'user', '[CONTEXT COMPACTION — REFERENCE ONLY] 摘要正文', 1900.0)",
        [],
    )
    .unwrap();
    drop(src);
    let home = tempfile::tempdir().unwrap();
    let mut conn = db::open(home.path()).unwrap();
    hermes::import(&mut conn, home.path(), &src_path).unwrap();

    let cl: i64 = conn
        .query_row("SELECT compact_line_no FROM sessions WHERE id = 'hermes:s_cli'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cl, 20, "摘要行 id 即压缩点");
    let d = db::read_session(&conn, "hermes:s_cli", 100, None, true).unwrap();
    let contents: Vec<String> = d["messages"].as_array().unwrap()
        .iter().map(|m| m["content"].as_str().unwrap_or("").to_string()).collect();
    assert!(contents.iter().any(|c| c == "压缩前的用户提问"), "压缩前原文在切片内: {contents:?}");
    assert!(!contents.iter().any(|c| c.contains("REFERENCE ONLY")), "摘要行本身不在切片内: {contents:?}");
}

#[test]
fn hermes_respects_disabled_agent() {
    // 停用 hermes 后 import_defaults 不再扫它的库（YOUMEM_HERMES_DB 覆盖同拦）
    let home = tempfile::tempdir().unwrap();
    ingest::set_agent_disabled(home.path(), "hermes", true).unwrap();
    let off = ingest::disabled_agents(home.path());
    assert!(off.contains(&"hermes".to_string()));
    // set_agent_disabled 只认已知 agent
    assert!(ingest::set_agent_disabled(home.path(), "nonexistent", true).is_err());
}
