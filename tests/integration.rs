//! End-to-end: fixture session files -> import -> search / context /
//! handoff -> vault export round-trip.

use std::path::Path;

use yourmem::{adapters, db, ingest, vault};

const CLAUDE_FIXTURE: &str = r#"{"type":"mode","mode":"normal","sessionId":"aaaa-1111"}
{"type":"user","cwd":"/tmp/proj-demo","gitBranch":"main","sessionId":"aaaa-1111","uuid":"u1","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"临江市城市内涝预警，水位用 ln(WL+1) 变换"}}
{"type":"assistant","uuid":"u2","timestamp":"2026-08-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"考虑特征工程"},{"type":"text","text":"好的，先做 LST 特征筛选"},{"type":"tool_use","name":"Bash","input":{"command":"python feature_selection.py"}}]}}
{"type":"user","uuid":"u3","timestamp":"2026-08-01T10:01:05Z","message":{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","text":"selected 12 features"}]}]}}"#;

const CODEX_FIXTURE: &str = r#"{"timestamp":"2026-08-02T09:00:00Z","type":"session_meta","payload":{"id":"bbbb-2222","cwd":"/tmp/proj-demo"}}
{"timestamp":"2026-08-02T09:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"高水位样本不要删除，继续内涝防治建模"}]}}
{"timestamp":"2026-08-02T09:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"高水位样本不要删除，继续内涝防治建模"}}
{"timestamp":"2026-08-02T09:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"明白，保留高水位样本"}]}}
{"timestamp":"2026-08-02T09:00:03Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":\"python train.py\"}"}}"#;

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let claude_dir = src.path().join("claude");
    let codex_dir = src.path().join("codex");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(claude_dir.join("aaaa-1111.jsonl"), format!("{CLAUDE_FIXTURE}\n")).unwrap();
    std::fs::write(
        codex_dir.join("rollout-2026-08-02T09-00-00-bbbb-2222-cccc-4444-dddd-5555eeee.jsonl"),
        format!("{CODEX_FIXTURE}\n"),
    )
    .unwrap();
    (home, src)
}

fn roots(src: &Path) -> Vec<(&'static str, std::path::PathBuf)> {
    vec![
        (adapters::AGENT_CLAUDE, src.join("claude")),
        (adapters::AGENT_CODEX, src.join("codex")),
    ]
}

#[test]
fn full_pipeline() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();

    // ---- import
    let outcome = ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    assert_eq!(outcome.files_seen, 2);
    assert_eq!(outcome.files_updated, 2);
    // claude: user + thinking + text + tool_call + tool_result = 5
    // codex: user + assistant + tool_call (event_msg dupe suppressed) = 3
    assert_eq!(outcome.messages_added, 8);

    // re-import is a no-op
    let again = ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    assert_eq!(again.messages_added, 0);

    // ---- project grouping: both sessions share one project
    let projects = db::list_projects(&conn).unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["name"], "proj-demo");
    assert_eq!(projects[0]["sessions"], 2);

    // ---- search: Chinese substring (2 chars -> LIKE fallback)
    let hits = db::search(&conn, &db::SearchOpts {
        query: "内涝".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 2); // one claude, one codex

    // FTS path (>=3 chars) with agent filter
    let hits = db::search(&conn, &db::SearchOpts {
        query: "LST".into(), project: None, agent: Some("claude".into()), kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["agent"], "claude");

    // ---- handoff + context
    let (pid, ..) = db::resolve_project(&conn, Some("demo"), None).unwrap().unwrap();
    let hid = db::create_handoff(&conn, pid, &db::HandoffFields {
        title: "特征筛选完成",
        done: "RF SelectFromModel 筛出 12 个特征",
        state: "待建模",
        decisions: "水位用 ln(WL+1)；高水位样本保留",
        files_changed: "feature_selection.py",
        open_issues: "",
        next_steps: "训练 GeoXAI 模型",
        session_id: Some("claude:aaaa-1111"),
    })
    .unwrap();
    assert!(hid > 0);

    let ctx = db::project_context(&conn, pid).unwrap();
    assert_eq!(ctx["latest_handoff"]["decisions"], "水位用 ln(WL+1)；高水位样本保留");
    assert_eq!(ctx["by_agent"].as_array().unwrap().len(), 2);

    // ---- vault export round-trip
    let out = home.path().join("exported.jsonl");
    let lines = vault::export_session(&conn, home.path(), "claude:aaaa-1111", &out).unwrap();
    assert_eq!(lines, 4);
    let rebuilt = std::fs::read_to_string(&out).unwrap();
    assert_eq!(rebuilt, format!("{CLAUDE_FIXTURE}\n"));

    let status = vault::status(&conn, home.path()).unwrap();
    assert_eq!(status["sessions_archived"], 2);
}

#[test]
fn incremental_append() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();

    // append a new line to the claude file
    let f = src.path().join("claude").join("aaaa-1111.jsonl");
    let mut content = std::fs::read_to_string(&f).unwrap();
    content.push_str("{\"type\":\"user\",\"uuid\":\"u9\",\"timestamp\":\"2026-08-01T11:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"GWR 结果怎么看\"}}\n");
    std::fs::write(&f, content).unwrap();

    let outcome = ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    assert_eq!(outcome.messages_added, 1);

    let hits = db::search(&conn, &db::SearchOpts {
        query: "GWR".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1);

    // vault now covers the appended line too
    let out = home.path().join("exported.jsonl");
    vault::export_session(&conn, home.path(), "claude:aaaa-1111", &out).unwrap();
    let rebuilt = std::fs::read_to_string(&out).unwrap();
    assert!(rebuilt.contains("GWR 结果怎么看"));
}

#[test]
fn memory_lifecycle() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    let (pid, ..) = db::resolve_project(&conn, Some("demo"), None).unwrap().unwrap();

    // decision -> suggested by default; fact -> confirmed
    let dec = db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "decision",
        content: "EC 高值样本不删除", status: None,
        source_session_id: Some("codex:bbbb-2222-cccc-4444-dddd-5555eeee"), source_message_id: None,
    })
    .unwrap();
    let fact = db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "fact",
        content: "临江市数据集共 1241 个样本", status: None,
        source_session_id: None, source_message_id: None,
    })
    .unwrap();
    let global = db::save_memory(&conn, &db::MemoryInput {
        project_id: None, scope: "global", r#type: "preference",
        content: "论文写作避免营销式表达", status: None,
        source_session_id: None, source_message_id: None,
    })
    .unwrap();

    // active list shows both suggested + confirmed; context sees global too
    let active = db::list_memories(&conn, &db::MemoryFilter {
        project_id: Some(pid), scope: None, r#type: None, status: None,
        agent: None, include_global: true, limit: 50,
    })
    .unwrap();
    assert_eq!(active.len(), 3);
    assert_eq!(active.iter().find(|m| m["id"] == dec).unwrap()["status"], "suggested");

    // confirm the decision, then it appears in project context
    db::update_memory_status(&conn, &dec, "confirm", None).unwrap();
    let ctx = db::project_context(&conn, pid).unwrap();
    let confirmed = ctx["memories"]["confirmed"].as_array().unwrap();
    assert_eq!(confirmed.len(), 3); // decision + fact + global preference

    // search memory (2-char Chinese -> LIKE fallback)
    let hits = db::search_memory(&conn, "内涝", &db::MemoryFilter {
        project_id: None, scope: None, r#type: None, status: None,
        agent: None, include_global: false, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 0); // no memory mentions 内涝 yet
    let hits = db::search_memory(&conn, "样本", &db::MemoryFilter {
        project_id: None, scope: None, r#type: None, status: None,
        agent: None, include_global: false, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 2);

    // supersede the fact with a newer one
    let fact2 = db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "fact",
        content: "临江市数据集更新为 1302 个样本", status: None,
        source_session_id: None, source_message_id: None,
    })
    .unwrap();
    db::update_memory_status(&conn, &fact, "supersede", Some(&fact2)).unwrap();
    let old = db::list_memories(&conn, &db::MemoryFilter {
        project_id: Some(pid), scope: None, r#type: None,
        status: Some("superseded".into()), agent: None, include_global: false, limit: 10,
    })
    .unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(old[0]["superseded_by"], fact2);
    // superseded memory no longer active
    let active = db::list_memories(&conn, &db::MemoryFilter {
        project_id: Some(pid), scope: None, r#type: None, status: None,
        agent: None, include_global: false, limit: 50,
    })
    .unwrap();
    assert!(!active.iter().any(|m| m["id"] == fact));

    // open tasks flow into get_recent_work data
    db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "task",
        content: "训练 GeoXAI 模型", status: None,
        source_session_id: None, source_message_id: None,
    })
    .unwrap();
    let tasks = db::open_tasks(&conn, Some(pid)).unwrap();
    assert_eq!(tasks.len(), 1);

    // invalid input rejected
    assert!(db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "nonsense",
        content: "x", status: None, source_session_id: None, source_message_id: None,
    })
    .is_err());
    // global id is a valid standalone memory
    assert!(global.starts_with("mem_"));
}

#[test]
fn lineage_detection() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let claude_dir = src.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();

    // parent session: 3 user messages (uuids p1, p2, p3)
    std::fs::write(claude_dir.join("parent-0000.jsonl"),
        "{\"type\":\"user\",\"uuid\":\"p1\",\"timestamp\":\"2026-08-01T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"第一条\"}}\n\
         {\"type\":\"user\",\"uuid\":\"p2\",\"timestamp\":\"2026-08-01T10:01:00Z\",\"message\":{\"role\":\"user\",\"content\":\"第二条\"}}\n\
         {\"type\":\"user\",\"uuid\":\"p3\",\"timestamp\":\"2026-08-01T10:02:00Z\",\"message\":{\"role\":\"user\",\"content\":\"第三条\"}}\n").unwrap();
    // continuation: first message's parentUuid is the parent's tail uuid
    std::fs::write(claude_dir.join("child-cont.jsonl"),
        "{\"type\":\"user\",\"parentUuid\":\"p3\",\"uuid\":\"c1\",\"timestamp\":\"2026-08-01T11:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"继续\"}}\n").unwrap();
    // fork: branches from the middle
    std::fs::write(claude_dir.join("child-fork.jsonl"),
        "{\"type\":\"user\",\"parentUuid\":\"p1\",\"uuid\":\"f1\",\"timestamp\":\"2026-08-01T12:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"分叉\"}}\n").unwrap();
    // compact: leading summary line with leafUuid
    std::fs::write(claude_dir.join("child-compact.jsonl"),
        "{\"type\":\"summary\",\"summary\":\"压缩摘要\",\"leafUuid\":\"p3\"}\n\
         {\"type\":\"user\",\"uuid\":\"s1\",\"timestamp\":\"2026-08-01T13:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"压缩后继续\"}}\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, src.path().join("claude"))];
    let outcome = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(outcome.lineage_links, 3);

    let cont = db::lineage_for(&conn, "claude:child-cont").unwrap();
    assert_eq!(cont["parents"][0]["type"], "continuation");
    assert_eq!(cont["parents"][0]["session_id"], "claude:parent-0000");

    let fork = db::lineage_for(&conn, "claude:child-fork").unwrap();
    assert_eq!(fork["parents"][0]["type"], "fork");

    let compact = db::lineage_for(&conn, "claude:child-compact").unwrap();
    assert_eq!(compact["parents"][0]["type"], "compact");

    // parent sees all three children
    let parent = db::lineage_for(&conn, "claude:parent-0000").unwrap();
    assert_eq!(parent["children"].as_array().unwrap().len(), 3);

    // read_session exposes lineage
    let sess = db::read_session(&conn, "claude:child-fork", 10, None, false).unwrap();
    assert_eq!(sess["lineage"]["parents"][0]["type"], "fork");

    // lineage_tree: transitive closure from any member reaches the whole family
    let tree = db::lineage_tree(&conn, "claude:child-fork").unwrap();
    let nodes = tree["nodes"].as_array().unwrap();
    let edges = tree["edges"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    assert_eq!(edges.len(), 3);
    assert!(nodes.iter().all(|n| n["ext"].is_null() && n["deleted"] == false));
    // 进度树口径：节点带 title（首条用户消息）与 tail（末条 assistant）
    let parent_node = nodes
        .iter()
        .find(|n| n["session_id"] == "claude:parent-0000")
        .unwrap();
    assert_eq!(parent_node["title"], "第一条");
    assert!(parent_node["tail"].is_null()); // 该会话无 assistant 消息
    // read_session carries the tree too
    assert_eq!(sess["lineage_tree"]["edges"].as_array().unwrap().len(), 3);
    // ext node: edge to a session missing from the table stays in the graph, flagged
    conn.execute(
        "INSERT INTO session_links(child_session_id, parent_session_id, link_type, created_at)
         VALUES ('claude:parent-0000', 'claude:gone-0000', 'continuation', '2026-08-01T09:00:00Z')",
        [],
    )
    .unwrap();
    let tree2 = db::lineage_tree(&conn, "claude:parent-0000").unwrap();
    let gone = tree2["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["session_id"] == "claude:gone-0000")
        .unwrap();
    assert_eq!(gone["ext"], true);
}

#[test]
fn codex_lineage_uses_native_parent_thread_metadata() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let codex_dir = src.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let parent = "11111111-1111-4111-8111-111111111111";
    let continuation = "22222222-2222-4222-8222-222222222222";
    let fork = "33333333-3333-4333-8333-333333333333";
    let subagent = "44444444-4444-4444-8444-444444444444";
    let write = |id: &str, payload: String| {
        std::fs::write(
            codex_dir.join(format!("rollout-2026-09-07T00-00-00-{id}.jsonl")),
            format!("{{\"type\":\"session_meta\",\"payload\":{payload}}}\n"),
        )
        .unwrap();
    };
    write(parent, format!(r#"{{"id":"{parent}","cwd":"/tmp/codex-lineage"}}"#));
    write(continuation, format!(r#"{{"id":"{continuation}","parent_thread_id":"{parent}"}}"#));
    write(fork, format!(r#"{{"id":"{fork}","forked_from_id":"{parent}"}}"#));
    // Codex writes fork metadata for subagents too; `source.subagent` is the
    // more specific relation and must win.
    write(subagent, format!(r#"{{"id":"{subagent}","forked_from_id":"{parent}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}"}}}}}}}}"#));

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CODEX, codex_dir)];
    let outcome = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(outcome.lineage_links, 3);
    for (id, expected) in [(continuation, "continuation"), (fork, "fork"), (subagent, "subagent")] {
        let lineage = db::lineage_for(&conn, &format!("codex:{id}")).unwrap();
        assert_eq!(lineage["parents"][0]["session_id"], format!("codex:{parent}"));
        assert_eq!(lineage["parents"][0]["type"], expected);
    }
}

#[test]
fn artifacts_and_db_snapshot() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let claude_dir = src.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("art-0000.jsonl"),
        "{\"type\":\"assistant\",\"uuid\":\"a1\",\"timestamp\":\"2026-08-01T10:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Write\",\"input\":{\"file_path\":\"/tmp/proj/feature_selection.py\",\"content\":\"...\"}}]}}\n\
         {\"type\":\"assistant\",\"uuid\":\"a2\",\"timestamp\":\"2026-08-01T10:01:00Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Write\",\"input\":{\"file_path\":\"/tmp/proj/feature_importance.png\",\"content\":\"...\"}}]}}\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, src.path().join("claude"))];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    let arts = db::list_artifacts(&conn, None, Some("claude:art-0000"), 10).unwrap();
    assert_eq!(arts.len(), 2);
    assert!(arts.iter().any(|a| a["path"] == "/tmp/proj/feature_importance.png"));

    // db snapshot + retention
    let r = vault::snapshot_db(&conn, home.path(), 2).unwrap();
    assert!(r["snapshot"].as_str().unwrap().ends_with(".sqlite"));
    vault::snapshot_db(&conn, home.path(), 2).unwrap();
    vault::snapshot_db(&conn, home.path(), 2).unwrap();
    assert!(vault::list_snapshots(home.path()).unwrap().len() <= 2);
}


#[test]
fn read_session_tail_and_preview() {
    let (home, src) = setup();
    let mut lines = String::new();
    for i in 1..=5u8 {
        lines.push_str(&format!(
            "{{\"type\":\"user\",\"uuid\":\"t{i}\",\"timestamp\":\"2026-08-01T10:0{i}:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"第{i}条\"}}}}\n"
        ));
    }
    std::fs::write(src.path().join("claude/long-0000.jsonl"), lines).unwrap();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();

    // 长会话：节选取尾部（最新消息）而不是头部
    let d = db::read_session(&conn, "claude:long-0000", 2, None, false).unwrap();
    assert_eq!(d["total_messages"], 5);
    assert_eq!(d["tail_excerpt"], true);
    let msgs = d["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0]["content"].as_str().unwrap().contains("第4条"));
    assert!(msgs[1]["content"].as_str().unwrap().contains("第5条"));

    // 短会话：全量返回
    let s = db::read_session(&conn, "claude:aaaa-1111", 60, None, false).unwrap();
    assert_eq!(s["tail_excerpt"], false);

    // 会话列表带首条用户消息摘要
    let sessions = db::recent_sessions(&conn, None, 10).unwrap();
    let long = sessions
        .iter()
        .find(|s| s["session_id"] == "claude:long-0000")
        .unwrap();
    assert_eq!(long["preview"].as_str().unwrap(), "第1条");
}

#[test]
fn truncated_source_file_is_reimported() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    let before = db::read_session(&conn, "claude:aaaa-1111", 60, None, false).unwrap();
    assert_eq!(before["total_messages"], 5);

    // Agent replaces the file with a shorter one (session reset / truncation):
    // file_len < imported_bytes must drop everything derived and re-import.
    let f = src.path().join("claude").join("aaaa-1111.jsonl");
    let old_len = std::fs::metadata(&f).unwrap().len();
    let short = "{\"type\":\"user\",\"uuid\":\"n1\",\"timestamp\":\"2026-08-03T09:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"重新开始，全新会话内容\"}}\n";
    assert!((short.len() as u64) < old_len, "fixture must actually shrink");
    std::fs::write(&f, short).unwrap();

    let outcome = ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    assert_eq!(outcome.messages_added, 1);

    // Old derived state is gone: session, search hits, vault manifest.
    let after = db::read_session(&conn, "claude:aaaa-1111", 60, None, false).unwrap();
    assert_eq!(after["total_messages"], 1);
    let stale = db::search(&conn, &db::SearchOpts {
        query: "LST".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(stale.len(), 0, "content from the replaced file must not survive");

    let out = home.path().join("exported.jsonl");
    let n = vault::export_session(&conn, home.path(), "claude:aaaa-1111", &out).unwrap();
    assert_eq!(n, 1);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), short);

    // and further appends still work on the re-imported file
    let mut content = String::from(short);
    content.push_str("{\"type\":\"user\",\"uuid\":\"n2\",\"timestamp\":\"2026-08-03T09:01:00Z\",\"message\":{\"role\":\"user\",\"content\":\"截断后的增量\"}}\n");
    std::fs::write(&f, content).unwrap();
    let more = ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    assert_eq!(more.messages_added, 1);
}

#[test]
fn vault_export_detects_corruption() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();

    // Tamper with one archived line on disk: export must fail loudly
    // (hash re-verification), never silently emit a corrupted file.
    let hash: String = conn
        .query_row(
            "SELECT hash FROM vault_lines WHERE session_id = 'claude:aaaa-1111' AND line_no = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    std::fs::write(vault::object_path(home.path(), &hash), b"tampered").unwrap();

    let out = home.path().join("exported.jsonl");
    assert!(vault::export_session(&conn, home.path(), "claude:aaaa-1111", &out).is_err());
}

#[test]
fn native_memory_files_backup() {
    use yourmem::memfiles;

    let home = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap(); // 模拟 $HOME 下的 agent 配置目录
    let claude_home = agents.path().join(".claude");
    let codex_home = agents.path().join(".codex");
    let proj = agents.path().join("proj-demo");

    // 全局文件
    std::fs::create_dir_all(&claude_home).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(claude_home.join("CLAUDE.md"), "全局规则：先跑测试再提交\n").unwrap();
    std::fs::write(codex_home.join("AGENTS.md"), "codex 全局指令\n").unwrap();
    // 项目文件 + Claude auto memory 目录（编码 = cwd 的分隔符转 -；JSON 字符串里
    // 的路径用正斜杠——Windows 反斜杠会让 \U 等成为非法 JSON 转义，整行被拒）
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "项目规则：decision 要确认\n").unwrap();
    std::fs::write(proj.join("AGENTS.md"), "项目 Codex 指令\n").unwrap();
    let enc = proj.to_string_lossy().replace(['/', '\\', ':'], "-");
    let mem_dir = claude_home.join("projects").join(&enc).join("memory");
    std::fs::create_dir_all(&mem_dir).unwrap();
    std::fs::write(mem_dir.join("MEMORY.md"), "内涝防治项目要点 v1\n").unwrap();

    // 导入一条 cwd 指向该项目的会话，驱动项目级路径发现
    let src = tempfile::tempdir().unwrap();
    let claude_dir = src.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("mem-0000.jsonl"),
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"uuid\":\"m1\",\"timestamp\":\"2026-08-01T10:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"你好\"}}}}\n",
            proj.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, src.path().join("claude"))];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    // §4: 会话详情暴露 native_id / 源文件路径 / resume 命令
    let sess = db::read_session(&conn, "claude:mem-0000", 10, None, false).unwrap();
    assert_eq!(sess["session"]["native_id"], "mem-0000");
    assert!(sess["session"]["file_path"].as_str().unwrap().ends_with("mem-0000.jsonl"));
    // 裸命令（Coffee CLI 式）；claude --resume 按项目目录查找，在项目目录下执行
    assert_eq!(sess["session"]["resume_command"], "claude --resume mem-0000");

    let dirs = memfiles::SourceDirs { claude: claude_home.clone(), codex: codex_home.clone() };

    // 首次采集：全局和项目指令及自动记忆各一个修订
    let out = memfiles::collect(&conn, home.path(), &dirs).unwrap();
    assert_eq!(out.files_monitored, 5);
    assert_eq!(out.revisions_added, 5);

    // 内容没变 → 不产生新修订
    let out = memfiles::collect(&conn, home.path(), &dirs).unwrap();
    assert_eq!(out.revisions_added, 0);

    let files = db::list_memory_files(&conn).unwrap();
    assert_eq!(files.len(), 5);
    let mem_md = files.iter().find(|f| f["path"].as_str().unwrap().ends_with("MEMORY.md")).unwrap();
    assert_eq!(mem_md["agent"], "claude");
    assert!(mem_md["scope"].as_str().unwrap().starts_with("project:"));
    assert_eq!(mem_md["revisions"], 1);
    let mem_id = mem_md["id"].as_i64().unwrap();

    // 修改 → 新修订；diff 可见变化；历史版本仍可取
    std::fs::write(mem_dir.join("MEMORY.md"), "内涝防治项目要点 v2：GWR 已完成\n").unwrap();
    let out = memfiles::collect(&conn, home.path(), &dirs).unwrap();
    assert_eq!(out.files_changed, 1);
    assert_eq!(out.revisions_added, 1);

    let shown = memfiles::show(&conn, home.path(), mem_id, None).unwrap();
    assert_eq!(shown["revisions"].as_array().unwrap().len(), 2);
    assert!(shown["content"].as_str().unwrap().contains("v2"));
    let r1 = shown["revisions"][1]["id"].as_i64().unwrap(); // 时间线倒序，[1] 是旧版
    let old = memfiles::show(&conn, home.path(), mem_id, Some(r1)).unwrap();
    assert!(old["content"].as_str().unwrap().contains("v1"));

    let d = memfiles::diff(&conn, home.path(), mem_id, 2).unwrap();
    let text = d["diffs"][0]["diff"].as_str().unwrap();
    assert!(text.contains("- 内涝防治项目要点 v1"));
    assert!(text.contains("+ 内涝防治项目要点 v2：GWR 已完成"));

    // 搜索：>=3 字符走 FTS，<3 字符走 LIKE 回退（同 db::search 约定）
    let hits = memfiles::search(&conn, home.path(), "内涝防治", 10).unwrap();
    assert_eq!(hits.len(), 1);
    let hits = memfiles::search(&conn, home.path(), "内涝", 10).unwrap();
    assert_eq!(hits.len(), 1);

    // agent 正在写的文件（UTF-8 被切断）留到下一轮
    std::fs::write(mem_dir.join("partial.md"), [0xe7, 0x9b]).unwrap(); // 半个"盐"
    let out = memfiles::collect(&conn, home.path(), &dirs).unwrap();
    assert_eq!(out.files_monitored, 6);
    assert_eq!(out.revisions_added, 0);
    // 写完整后下一轮采集到
    std::fs::write(mem_dir.join("partial.md"), "内涝防治补充\n").unwrap();
    let out = memfiles::collect(&conn, home.path(), &dirs).unwrap();
    assert_eq!(out.revisions_added, 1);

    // MCP read_native_memory 的底层：按 agent + 路径后缀取最新内容
    let r = memfiles::read_native(&conn, home.path(), "claude", Some("MEMORY.md")).unwrap();
    let arr = r["memory_files"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert!(arr[0]["content"].as_str().unwrap().contains("v2"));
    let all = memfiles::read_native(&conn, home.path(), "codex", None).unwrap();
    assert_eq!(all["memory_files"].as_array().unwrap().len(), 2);
}


#[test]
fn soft_delete_trash_roundtrip() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();
    let q = |c: &rusqlite::Connection| {
        db::search(c, &db::SearchOpts {
            query: "内涝".into(), project: None, agent: None, kind: None, limit: 10,
        })
        .unwrap()
    };
    assert_eq!(q(&conn).len(), 2);

    // 软删：搜索 / 最近会话 / 详情 / 项目计数全部隐藏
    db::set_session_deleted(&conn, "claude:aaaa-1111", true).unwrap();
    let hits = q(&conn);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["agent"], "codex");
    assert!(db::recent_sessions(&conn, None, 10)
        .unwrap()
        .iter()
        .all(|s| s["session_id"] != "claude:aaaa-1111"));
    assert!(db::read_session(&conn, "claude:aaaa-1111", 10, None, false).is_err());
    assert_eq!(db::list_projects(&conn).unwrap()[0]["sessions"], 1);
    let (pid, ..) = db::resolve_project(&conn, Some("demo"), None).unwrap().unwrap();
    assert_eq!(db::project_context(&conn, pid).unwrap()["by_agent"].as_array().unwrap().len(), 1);
    assert_eq!(db::stats(&conn).unwrap()["sessions_trash"], 1);

    // 回收站可见；重复删除 / 恢复未删会话都报错（幂等防呆）
    let trash = db::trash_sessions(&conn).unwrap();
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0]["session_id"], "claude:aaaa-1111");
    assert!(trash[0]["deleted_at"].is_string());
    assert!(db::set_session_deleted(&conn, "claude:aaaa-1111", true).is_err());
    assert!(db::set_session_deleted(&conn, "codex:bbbb-2222", false).is_err());
    assert!(db::set_session_deleted(&conn, "claude:nope", true).is_err());

    // 恢复：FTS 行从未被动过，恢复后原样可搜可见
    db::set_session_deleted(&conn, "claude:aaaa-1111", false).unwrap();
    assert!(db::trash_sessions(&conn).unwrap().is_empty());
    assert_eq!(q(&conn).len(), 2);
    assert!(db::read_session(&conn, "claude:aaaa-1111", 10, None, false).is_ok());
    assert_eq!(db::list_projects(&conn).unwrap()[0]["sessions"], 2);
}

#[test]
fn migrate_v3_to_v4_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    {
        // 模拟 0.3.1 的 v3 库：摘掉 deleted_at、版本号回写 3
        let conn = db::open(home.path()).unwrap();
        conn.execute_batch("ALTER TABLE sessions DROP COLUMN deleted_at; PRAGMA user_version = 3;")
            .unwrap();
    }
    let conn = db::open(home.path()).unwrap();
    let v: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, db::SCHEMA_VERSION);
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'deleted_at'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);
    // 再开一次走 fast path（version >= SCHEMA_VERSION 直接返回），幂等不报错
    db::open(home.path()).unwrap();
}

#[test]
fn cli_session_commands_log_usage() {
    let home = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_yourmem");
    for args in [vec!["session", "trash"], vec!["session", "delete", "claude:nope"]] {
        let out = std::process::Command::new(bin)
            .args(args)
            .env("YOUMEM_HOME", home.path())
            .output()
            .unwrap();
        // trash 成功；delete 不存在的会话报错退出——但两者都应已埋点
        let _ = out.status;
    }
    let conn = db::open(home.path()).unwrap();
    let log = db::usage_summary(&conn, 7).unwrap();
    assert!(log.iter().any(|u| u["source"] == "cli" && u["name"] == "session trash"));
    assert!(log.iter().any(|u| u["source"] == "cli" && u["name"] == "session delete"));
}

#[test]
fn mcp_initialize_imports_and_stays_healthy() {
    let (home, src) = setup();
    // 根目录用 env 覆盖指向 fixture，避免碰真实 ~/.claude
    std::env::set_var("YOUMEM_CLAUDE_DIR", src.path().join("claude"));
    std::env::set_var("YOUMEM_CODEX_DIR", src.path().join("codex"));
    std::env::set_var("YOUMEM_ZCODE_DIR", src.path().join("nope-rollout"));
    std::env::set_var("YOUMEM_KIMI_DIR", src.path().join("nope-sessions"));
    std::env::set_var("YOUMEM_OPENCODE_DB", src.path().join("nope.db"));
    std::env::set_var("YOUMEM_HERMES_DB", src.path().join("nope-hermes.db"));

    let r = yourmem::mcp::handle(home.path(), &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
    assert_eq!(r["result"]["serverInfo"]["name"], "yourmem");

    // initialize 顺带导入：新会话立即可搜
    let conn = db::open(home.path()).unwrap();
    let hits = db::search(&conn, &db::SearchOpts {
        query: "内涝".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 2);
    // 埋点口径：source=mcp, name=import_on_init
    let log = db::usage_summary(&conn, 7).unwrap();
    assert_eq!(
        log.iter().filter(|u| u["source"] == "mcp" && u["name"] == "import_on_init").count(),
        1
    );
    assert!(home.path().join(".last_import").exists());

    // 单飞：60 秒内第二次 initialize 不再导入（mtime 不变、usage 不增）
    let mtime = std::fs::metadata(home.path().join(".last_import")).unwrap().modified().unwrap();
    yourmem::mcp::handle(home.path(), &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"initialize"})).unwrap();
    let mtime2 = std::fs::metadata(home.path().join(".last_import")).unwrap().modified().unwrap();
    assert_eq!(mtime, mtime2);
    let log2 = db::usage_summary(&conn, 7).unwrap();
    assert_eq!(
        log2.iter().filter(|u| u["source"] == "mcp" && u["name"] == "import_on_init").count(),
        1
    );

    // import 失败不传导：库文件损坏时 initialize / tools/list 照常响应
    let bad = tempfile::tempdir().unwrap();
    std::fs::write(bad.path().join("yourmem.db"), b"not sqlite").unwrap();
    let r = yourmem::mcp::handle(bad.path(), &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
    assert_eq!(r["result"]["serverInfo"]["name"], "yourmem");
    let l = yourmem::mcp::handle(bad.path(), &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
    assert_eq!(l["result"]["tools"].as_array().unwrap().len(), 13);

    std::env::remove_var("YOUMEM_CLAUDE_DIR");
    std::env::remove_var("YOUMEM_CODEX_DIR");
    std::env::remove_var("YOUMEM_ZCODE_DIR");
    std::env::remove_var("YOUMEM_KIMI_DIR");
    std::env::remove_var("YOUMEM_OPENCODE_DB");
}

#[test]
fn codex_event_msg_suppressed_across_chunk_boundary() {
    // v7 回归：response_item 行与它的 event_msg 副本被两次增量导入切开时，
    // 第二个 chunk 自身不含 response_item——抑制必须依赖 source_files 里
    // 持久化的 saw_response_item 标志，否则副本被当老格式再收一遍
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dir = src.path().join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("rollout-2026-08-02T10-00-00-cccc-3333-dddd-4444-eeee-5555ffff.jsonl");
    let l1 = r#"{"timestamp":"2026-08-02T10:00:00Z","type":"session_meta","payload":{"id":"cccc-3333","cwd":"/tmp/proj-x"}}
{"timestamp":"2026-08-02T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"跨边界去重检查"}]}}
"#;
    std::fs::write(&f, l1).unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CODEX, dir.clone())];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    let l2 = r#"{"timestamp":"2026-08-02T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"跨边界去重检查"}}
"#;
    std::fs::write(&f, format!("{l1}{l2}")).unwrap();
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out.messages_added, 0, "跨 chunk 的 event_msg 副本必须被抑制");

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages WHERE session_id LIKE 'codex:%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

// S16 回归：LIKE 回退保持 AND 语义——混合长度多词查询（"内涝 ln" 的 "ln" <3 字符
// 整体走回退）不得要求词项字面相邻；命中项须同时含全部词项
#[test]
fn search_like_fallback_and_semantics() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();

    // claude 首条同时含 "内涝" 与 "ln"；codex 首条只含 "内涝" → AND 只命中 claude
    let hits = db::search(&conn, &db::SearchOpts {
        query: "内涝 ln".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1, "AND 语义：只有同时含两词的消息命中: {hits:?}");
    assert_eq!(hits[0]["agent"], "claude");

    // search_memory 同款回退
    let (pid, ..) = db::resolve_project(&conn, Some("demo"), None).unwrap().unwrap();
    db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "decision",
        content: "EC 高值样本用 ln 变换", status: None,
        source_session_id: None, source_message_id: None,
    })
    .unwrap();
    db::save_memory(&conn, &db::MemoryInput {
        project_id: Some(pid), scope: "project", r#type: "decision",
        content: "EC 高值样本不删除", status: None,
        source_session_id: None, source_message_id: None,
    })
    .unwrap();
    let hits = db::search_memory(&conn, "样本 ln", &db::MemoryFilter {
        project_id: None, scope: None, r#type: None, status: None,
        agent: None, include_global: false, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1, "memory 搜索回退同样是 AND: {hits:?}");
    assert!(hits[0]["content"].as_str().unwrap().contains("ln 变换"));
}

// agent 维度三端区分（2026-08-24 特性）：projects 返回 agent 构成、
// 记忆按来源会话的 agent 过滤、自定义根 CRUD 与五源检测
#[test]
fn agent_dimension_projects_memory_and_extra_roots() {
    let (home, src) = setup();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots(src.path()), None, None).unwrap();

    // projects 带 agents 构成（claude + codex 同项目）
    let projects = db::list_projects(&conn).unwrap();
    let agents = projects[0]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().any(|a| a == "claude") && agents.iter().any(|a| a == "codex"));

    // 两条记忆分别挂 claude / codex 会话来源，一条无来源
    let (pid, ..) = db::resolve_project(&conn, Some("demo"), None).unwrap().unwrap();
    let codex_sid: String = conn
        .query_row("SELECT id FROM sessions WHERE agent = 'codex'", [], |r| r.get(0))
        .unwrap();
    let mk = |content: &str, sid: Option<&str>| {
        db::save_memory(&conn, &db::MemoryInput {
            project_id: Some(pid), scope: "project", r#type: "fact",
            content, status: None, source_session_id: sid, source_message_id: None,
        })
        .unwrap()
    };
    mk("来自 claude 的记忆", Some("claude:aaaa-1111"));
    mk("来自 codex 的记忆", Some(&codex_sid));
    mk("手动记录无来源", None);
    let f = |agent: Option<String>| db::MemoryFilter {
        project_id: None, scope: None, r#type: None, status: Some("all".into()),
        agent, include_global: false, limit: 50,
    };
    let claude_mems = db::list_memories(&conn, &f(Some("claude".into()))).unwrap();
    assert_eq!(claude_mems.len(), 1);
    assert_eq!(claude_mems[0]["content"], "来自 claude 的记忆");
    assert_eq!(claude_mems[0]["source_agent"], "claude");
    let codex_mems = db::list_memories(&conn, &f(Some("codex".into()))).unwrap();
    assert_eq!(codex_mems.len(), 1, "无来源指针的记忆不属于任何 agent");
    // search_memory 同口径
    let hits = db::search_memory(&conn, "记忆", &f(Some("codex".into()))).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["source_agent"], "codex");

    // 自定义根 CRUD：校验、去重、持久化、删除
    let extra = tempfile::tempdir().unwrap();
    ingest::add_extra_root(home.path(), "claude", extra.path()).unwrap();
    assert!(ingest::add_extra_root(home.path(), "claude", extra.path()).is_err(), "重复登记必须报错");
    assert!(ingest::add_extra_root(home.path(), "opencode", extra.path()).is_err(), "opencode 是单库源");
    assert!(ingest::add_extra_root(home.path(), "claude", std::path::Path::new("/nonexistent-dir")).is_err());
    assert_eq!(ingest::extra_roots(home.path()).len(), 1);
    let det = ingest::agent_sources(home.path(), &conn).unwrap();
    let claude = det["agents"].as_array().unwrap().iter().find(|a| a["agent"] == "claude").unwrap();
    assert_eq!(claude["sessions"], 1);
    assert_eq!(claude["extra_roots"].as_array().unwrap().len(), 1);
    assert!(det["agents"].as_array().unwrap().iter()
        .any(|a| a["agent"] == "opencode" && a["can_add_root"] == false));
    ingest::remove_extra_root(home.path(), "claude", extra.path()).unwrap();
    assert!(ingest::extra_roots(home.path()).is_empty());
    assert!(ingest::remove_extra_root(home.path(), "claude", extra.path()).is_err(), "删不存在的根必须报错");

    // 同一路径不同 agent 都要采集：合并去重键必须是 (agent, path)——按 path 去重
    // 会静默丢掉第二个 agent 的根（codex 评审 blocker）。合并逻辑的键语义在
    // ingest::tests::extra_root_dedup_key_is_agent_plus_path 单测；这里验登记层
    let shared = tempfile::tempdir().unwrap();
    ingest::add_extra_root(home.path(), "claude", shared.path()).unwrap();
    ingest::add_extra_root(home.path(), "kimi", shared.path()).unwrap();
    assert_eq!(ingest::extra_roots(home.path()).len(), 2, "同路径不同 agent 应并存");
    ingest::remove_extra_root(home.path(), "claude", shared.path()).unwrap();
    ingest::remove_extra_root(home.path(), "kimi", shared.path()).unwrap();
    assert!(ingest::extra_roots(home.path()).is_empty());

    // 停用 agent（用户反馈 2026-08-29：卸载 opencode 后不该再扫它的源）：
    // 停用后 import_defaults 跳过该源、agent_sources 报 disabled
    ingest::set_agent_disabled(home.path(), "opencode", true).unwrap();
    ingest::set_agent_disabled(home.path(), "claude", true).unwrap();
    assert!(ingest::set_agent_disabled(home.path(), "nonexistent", true).is_err(), "未知 agent 拒绝");
    let det = ingest::agent_sources(home.path(), &conn).unwrap();
    let oc = det["agents"].as_array().unwrap().iter().find(|a| a["agent"] == "opencode").unwrap();
    assert_eq!(oc["disabled"], true, "opencode 应报停用");
    let claude_row = det["agents"].as_array().unwrap().iter().find(|a| a["agent"] == "claude").unwrap();
    assert_eq!(claude_row["disabled"], true);
    assert!(det["watchlist"].as_array().unwrap().len() >= 5, "观察名单要有主流 agent");
    ingest::set_agent_disabled(home.path(), "claude", false).unwrap();
    let det2 = ingest::agent_sources(home.path(), &conn).unwrap();
    let claude2 = det2["agents"].as_array().unwrap().iter().find(|a| a["agent"] == "claude").unwrap();
    assert_eq!(claude2["disabled"], false, "重新启用生效");
}


#[test]
fn mcp_tool_descriptions_within_token_budget() {
    // §12.3 机制化（engramory 有界容量吸收）：工具定义+描述是每会话固定
    // token 成本——总量超预算必须显式裁定，不许默默膨胀。
    let l = yourmem::mcp::handle(std::path::Path::new("/nonexistent-home"), &serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list"
    }));
    // 无库环境 tools/list 正常返回（不依赖 DB）
    if let Some(resp) = l {
        let total: usize = serde_json::to_string(&resp["result"]["tools"]).unwrap().len();
        // 基线 7916 字符（13 工具，2026-08-29 serde 实测钉死）。守门语义：只降不升——
        // 新增工具须在此预算内腾挪（压旧描述或合并工具），膨胀必须显式裁定并更新此值
        assert!(total <= 7916, "工具定义总量 {total} 超预算（§12.3：只降不升；新工具先回答值多少 tokens）");
    }
}

#[test]
fn migrate_v6_to_v7_restores_saw_response_item() {
    // codex 评审：v3→v4 测试只是摘列回加，没构造真正的 v6 库——v7 的
    // saw_response_item 迁移（source_files 加列）此前零覆盖
    let home = tempfile::tempdir().unwrap();
    {
        let conn = db::open(home.path()).unwrap();
        conn.execute_batch(
            "ALTER TABLE source_files DROP COLUMN saw_response_item; PRAGMA user_version = 6;",
        )
        .unwrap();
    }
    let conn = db::open(home.path()).unwrap();
    let v: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, db::SCHEMA_VERSION);
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('source_files') WHERE name = 'saw_response_item'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);
    db::open(home.path()).unwrap(); // fast path 幂等
}

#[test]
fn migrate_v9_to_v10_adds_compact_line_no() {
    // v10（0.3.8）：压缩点列。老库升 v10 后列必须存在（ALTER 幂等）；NULL 语义
    // 由 ingest 首次检测填充，读取侧按 NULL=未压缩处理。
    let home = tempfile::tempdir().unwrap();
    {
        let conn = db::open(home.path()).unwrap();
        conn.execute_batch(
            "ALTER TABLE sessions DROP COLUMN compact_line_no; PRAGMA user_version = 9;",
        )
        .unwrap();
    }
    let conn = db::open(home.path()).unwrap();
    let v: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, db::SCHEMA_VERSION);
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'compact_line_no'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn migrate_v8_rebuild_keeps_cursor_text() {
    // 真机事故（2026-08-29）：v8 表重建的定义漏了 cursor_text，真实旧库升 v8 后
    // 列与 opencode 决胜游标数据一起消失，opencode 导入直接报 no such column。
    // 两个层面都要钉死：重建后的表必须有该列；已 stamp 到 v8 的受损库要被 v9 自愈。
    let home = tempfile::tempdir().unwrap();
    {
        // 场景 A：v7 旧库（含 cursor_text 数据）升 v8——重建不得丢列丢数据
        let conn = db::open(home.path()).unwrap();
        conn.execute(
            "INSERT INTO source_files(path, agent, imported_bytes, line_count, cursor_text, saw_response_item, updated_at)
             VALUES ('/x/a.jsonl', 'opencode', 10, 2, '42', 0, '2026-08-29T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version = 7;").unwrap();
    }
    let conn = db::open(home.path()).unwrap();
    let (ct, ib): (Option<String>, i64) = conn
        .query_row(
            "SELECT cursor_text, imported_bytes FROM source_files WHERE agent = 'opencode'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((ct.as_deref(), ib), (Some("42"), 10), "重建必须保留 cursor_text 列与数据");
    drop(conn);

    // 场景 B：已被 v8 破坏的库（stamp 8、复合主键、缺 cursor_text）——v9 自愈
    let broken = tempfile::tempdir().unwrap();
    {
        let conn = db::open(broken.path()).unwrap();
        conn.execute_batch(
            "DROP TABLE source_files;
             CREATE TABLE source_files (
               path TEXT NOT NULL, agent TEXT NOT NULL,
               imported_bytes INTEGER NOT NULL DEFAULT 0,
               line_count INTEGER NOT NULL DEFAULT 0,
               saw_response_item INTEGER NOT NULL DEFAULT 0,
               updated_at TEXT NOT NULL,
               PRIMARY KEY (agent, path)
             );
             PRAGMA user_version = 8;",
        )
        .unwrap();
    }
    let conn = db::open(broken.path()).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('source_files') WHERE name = 'cursor_text'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "受损 v8 库必须被 v9 补回 cursor_text");
    drop(conn);
    db::open(broken.path()).unwrap(); // fast path 幂等
}

#[test]
fn find_similar_flags_duplicates_not_distinct() {
    // 写路径纪律（engramory 吸收）：同文/近文命中，不同主题不误报
    let home = tempfile::tempdir().unwrap();
    let conn = yourmem::db::open(home.path()).unwrap();
    let save = |c: &str| {
        yourmem::db::save_memory(&conn, &yourmem::db::MemoryInput {
            project_id: None, scope: "global", r#type: "fact", content: c,
            status: None, source_session_id: None, source_message_id: None,
        }).unwrap()
    };
    save("项目采用 SQLite 加 FTS5 trigram 做全文检索");
    save("备份策略是 VACUUM INTO 快照加内容寻址对象库");
    let dup = yourmem::db::find_similar(&conn, "项目采用 SQLite 加 FTS5 trigram 做全文检索（重申）", None, "fact").unwrap();
    assert!(!dup.is_empty(), "近文必须命中: {dup:?}");
    let distinct = yourmem::db::find_similar(&conn, "今天天气很好适合散步遛狗晒太阳", None, "fact").unwrap();
    assert!(distinct.is_empty(), "不同主题不得误报: {distinct:?}");
    // 类型不同不算重复
    let other_type = yourmem::db::find_similar(&conn, "项目采用 SQLite 加 FTS5 trigram 做全文检索", None, "decision").unwrap();
    assert!(other_type.is_empty(), "跨类型不提示: {other_type:?}");

    let (id, similar) = yourmem::db::save_memory_with_similar(&conn, &yourmem::db::MemoryInput {
        project_id: None, scope: "global", r#type: "fact",
        content: "项目采用 SQLite 加 FTS5 trigram 做全文检索",
        status: None, source_session_id: None, source_message_id: None,
    }).unwrap();
    assert!(!similar.is_empty());
    assert!(similar.iter().all(|m| m["id"] != id), "查重必须在写入前完成");
    let saved: String = conn.query_row("SELECT status FROM memories WHERE id = ?1", [&id], |r| r.get(0)).unwrap();
    assert_eq!(saved, "confirmed");
}

#[test]
fn project_add_archive_restore_lifecycle() {
    // 项目手动登记 + 废弃（软归档）：列表全端隐藏、stats 不计、
    // 导入不复活（upsert_project 只碰 updated_at）、可恢复、幂等防呆。
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();

    // 手动登记：名字取路径 basename；重复登记幂等返回同一行
    let (id1, created1) = db::add_project(&conn, "/tmp/proj-demo/").unwrap();
    assert!(created1);
    let (id2, created2) = db::add_project(&conn, "/tmp/proj-demo").unwrap();
    assert!(!created2 && id2 == id1, "重复登记必须幂等");
    let row = db::list_projects(&conn).unwrap().into_iter().next().unwrap();
    assert_eq!(row["name"], "proj-demo");

    // 模拟 import 在归档前后 upsert 同路径项目：归档标记不得被碰掉
    db::set_project_archived(&conn, id1, true).unwrap();
    db::upsert_project(&conn, "/tmp/proj-demo", "proj-demo").unwrap();
    assert!(db::list_projects(&conn).unwrap().is_empty(), "归档项目必须从活跃列表消失");
    assert_eq!(db::list_archived_projects(&conn).unwrap().len(), 1);

    // stats 计数排除归档项目
    let st = db::stats(&conn).unwrap();
    assert_eq!(st["projects"], 0);

    // 幂等防呆：重复废弃/重复恢复都报错（同 set_session_deleted）
    assert!(db::set_project_archived(&conn, id1, true).is_err());
    assert!(db::set_project_archived(&conn, 9999, false).is_err());

    // 恢复后回到活跃列表；重登记已归档路径 = 顺带恢复
    db::set_project_archived(&conn, id1, false).unwrap();
    assert_eq!(db::list_projects(&conn).unwrap().len(), 1);
    db::set_project_archived(&conn, id1, true).unwrap();
    let (id3, created3) = db::add_project(&conn, "/tmp/proj-demo").unwrap();
    assert!(!created3 && id3 == id1, "重登记归档路径返回原项目");
    assert_eq!(db::list_projects(&conn).unwrap().len(), 1, "重登记即恢复");
}
