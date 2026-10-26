use serde_json::{json, Value};
use yourmem::{adapters, db, ingest, work};

fn mcp(home: &std::path::Path, id: i64, name: &str, arguments: Value) -> Value {
    let response = yourmem::mcp::handle(
        home,
        &json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }),
    )
    .unwrap();
    assert_ne!(response["result"]["isError"], true, "{name}: {response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn claude_codex_kimi_native_formats_continue_one_task() {
    let home = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let project = sources.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let claude_dir = sources.path().join("claude");
    let codex_dir = sources.path().join("codex");
    let kimi_dir = sources.path().join("kimi");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&codex_dir).unwrap();

    let claude_file = claude_dir.join("claude-task.jsonl");
    let write = |path: &std::path::Path, lines: &[Value]| {
        std::fs::write(
            path,
            lines
                .iter()
                .map(|line| line.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
    };
    write(
        &claude_file,
        &[
            json!({"type":"user","cwd":project,"uuid":"c1","timestamp":"2026-10-25T01:00:00Z","message":{"role":"user","content":"实现跨 Agent 任务节点"}}),
            json!({"type":"assistant","cwd":project,"uuid":"c2","timestamp":"2026-10-25T01:01:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"claude-write","name":"Write","input":{"file_path":project.join("src/task.rs"),"content":"task"}}]}}),
            json!({"type":"user","cwd":project,"uuid":"c3","timestamp":"2026-10-25T01:01:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"claude-write","content":"ok","is_error":false}]}}),
        ],
    );
    let roots = vec![
        (adapters::AGENT_CLAUDE, claude_dir.clone()),
        (adapters::AGENT_CODEX, codex_dir.clone()),
        (adapters::AGENT_KIMI, kimi_dir.clone()),
    ];
    let mut conn = db::open(home.path()).unwrap();
    let imported = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert!(
        imported.errors.is_empty(),
        "import errors: {:?}; file={}",
        imported.errors,
        claude_file.display()
    );
    assert!(
        imported.messages_added >= 3,
        "{}",
        ingest::outcome_json(&imported)
    );
    let pid: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE path=?1",
            [project.to_string_lossy().as_ref()],
            |r| r.get(0),
        )
        .unwrap();
    db::set_project_archived(&conn, pid, false).unwrap();
    let task = mcp(
        home.path(),
        1,
        "create_task",
        json!({
            "project":project,"title":"实现跨 Agent 任务节点","session_id":"claude:claude-task"
        }),
    )["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    mcp(
        home.path(),
        2,
        "save_memory",
        json!({
            "project":project,"type":"decision","content":"对话作为证据，持久任务作为主线",
            "session_id":"claude:claude-task"
        }),
    );

    let codex_file =
        codex_dir.join("rollout-2026-10-25T02-00-00-11111111-1111-4111-8111-111111111111.jsonl");
    write(
        &codex_file,
        &[
            json!({"timestamp":"2026-10-25T02:00:00Z","type":"session_meta","payload":{"id":"11111111-1111-4111-8111-111111111111","cwd":project}}),
            json!({"timestamp":"2026-10-25T02:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"继续实现跨 Agent 任务节点"}]}}),
            json!({"timestamp":"2026-10-25T02:00:02Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","call_id":"codex-edit","arguments":"*** Begin Patch\n*** Update File: src/task.rs\n@@\n*** End Patch"}}),
            json!({"timestamp":"2026-10-25T02:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"codex-edit","output":{"exit_code":0}}}),
        ],
    );
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let codex_sid = "codex:11111111-1111-4111-8111-111111111111";
    let read = work::task_context(&conn, home.path(), &task, 50).unwrap();
    assert!(
        !read["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session_id"] == codex_sid),
        "读取上下文不得自动关联"
    );
    let closed = mcp(
        home.path(),
        3,
        "close_work",
        json!({
            "project":project,"task_id":task,"title":"Codex 修改完成","next_steps":"Kimi 复核","status":"completed"
        }),
    );
    let token = closed["claim"]["claim_token"].as_str().unwrap().to_string();
    let arguments = json!({"task_id":task,"claim_token":token});
    let extra: Vec<Value> = vec![
        json!({"timestamp":"2026-10-25T02:01:00Z","type":"response_item","payload":{"type":"function_call","name":"mcp__yourmem__get_task_context","call_id":"codex-claim","arguments":arguments.to_string()}}),
        json!({"timestamp":"2026-10-25T02:01:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"codex-claim","output":"{\"ok\":true}"}}),
    ];
    std::fs::write(
        &codex_file,
        std::fs::read_to_string(&codex_file).unwrap()
            + &extra
                .iter()
                .map(|line| line.to_string() + "\n")
                .collect::<String>(),
    )
    .unwrap();

    let kimi_session = kimi_dir.join("wd_test/session_22222222-2222-4222-8222-222222222222");
    std::fs::create_dir_all(kimi_session.join("agents/main")).unwrap();
    std::fs::write(
        kimi_session.join("state.json"),
        json!({"cwd":project}).to_string(),
    )
    .unwrap();
    write(
        &kimi_session.join("agents/main/wire.jsonl"),
        &[
            json!({"type":"turn.prompt","time":1761392820000i64,"input":[{"type":"text","text":format!("继续复核\nyourmem-task:{task}")}]}),
            json!({"type":"context.append_loop_event","time":1761392821000i64,"event":{"type":"tool.call","id":"kimi-read","name":"mcp__yourmem__get_task_context","args":{"task_id":task}}}),
        ],
    );
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let kimi_sid = "kimi:session_22222222-2222-4222-8222-222222222222";
    let context = work::task_context(&conn, home.path(), &task, 100).unwrap();
    let sessions: std::collections::HashSet<String> = context["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        sessions,
        ["claude:claude-task", codex_sid, kimi_sid]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    assert_eq!(context["artifacts"].as_array().unwrap().len(), 2);
    assert_eq!(context["handoffs"][0]["next_steps"], "Kimi 复核");
    assert_eq!(context["task"]["status"], "completed");
    assert_eq!(context["task"]["status_source"], "agent_reported");
    assert!(
        context["verifications"].as_array().unwrap().is_empty(),
        "Agent 自报完成不得变成验证"
    );
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT status FROM task_claims", [], |r| r.get(0))
            .unwrap(),
        "claimed"
    );
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM task_sessions WHERE session_id=?1 AND evidence='native_tool_claim'", [codex_sid], |r| r.get(0)).unwrap(), 1);
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM task_sessions WHERE session_id=?1 AND evidence='explicit_handoff'", [kimi_sid], |r| r.get(0)).unwrap(), 1);
    let timeline =
        work::timeline(&conn, Some(pid), Some(&task), None, None, &[], None, 100).unwrap();
    assert!(timeline["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "artifact"));
    assert!(timeline["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "memory_changed"));
    assert!(work::task_markdown(&conn, home.path(), &task)
        .unwrap()
        .contains("Codex 修改完成"));

    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artifact_events", [], |r| r.get(0))
            .unwrap(),
        2,
        "重复采集不能双录"
    );
}

#[test]
fn real_formats_verify_tests_and_scope_by_explicit_project_only() {
    let home = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let claude_dir = sources.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let alpha = sources.path().join("alpha");
    let beta = sources.path().join("beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    let write = |name: &str, lines: &[Value]| {
        std::fs::write(
            claude_dir.join(name),
            lines.iter().map(|l| l.to_string() + "\n").collect::<String>(),
        )
        .unwrap();
    };
    write(
        "alpha-run.jsonl",
        &[
            json!({"type":"user","cwd":alpha,"uuid":"a1","timestamp":"2026-10-25T01:00:00Z","message":{"role":"user","content":"跑测试"}}),
            json!({"type":"assistant","cwd":alpha,"uuid":"a2","timestamp":"2026-10-25T01:01:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"alpha-test","name":"Bash","input":{"command":"cargo test --all"}}]}}),
            json!({"type":"user","cwd":alpha,"uuid":"a3","timestamp":"2026-10-25T01:02:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"alpha-test","content":"test result: ok","is_error":false}]}}),
        ],
    );
    // beta 更晚活跃：旧的"最近项目"回退会把无项目查询收窄到 beta
    write(
        "beta-run.jsonl",
        &[json!({"type":"user","cwd":beta,"uuid":"b1","timestamp":"2026-10-26T01:00:00Z","message":{"role":"user","content":"另一个项目"}})],
    );
    let roots = vec![(adapters::AGENT_CLAUDE, claude_dir.clone())];
    let mut conn = db::open(home.path()).unwrap();
    let imported = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert!(imported.errors.is_empty(), "{:?}", imported.errors);
    for path in [&alpha, &beta] {
        let pid: i64 = conn
            .query_row("SELECT id FROM projects WHERE path=?1", [path.to_string_lossy().as_ref()], |r| r.get(0))
            .unwrap();
        db::set_project_archived(&conn, pid, false).unwrap();
    }

    let created = mcp(
        home.path(),
        1,
        "create_task",
        json!({"project":alpha,"title":"验证真实格式","session_id":"claude:not-imported-yet"}),
    );
    assert_eq!(created["session_pending"], true, "未采集的当前会话不得阻断建任务");
    let task = created["task"]["id"].as_str().unwrap().to_string();
    mcp(home.path(), 2, "save_memory", json!({"project":alpha,"type":"lesson","content":"alpha 专属经验"}));

    // 测试进程 cwd 不属于任何项目：完整 task_id 与无项目搜索都不得被收窄
    let context = mcp(home.path(), 3, "get_task_context", json!({"task_id":task}));
    assert_eq!(context["task"]["id"], task);
    let found = mcp(home.path(), 4, "search_memory", json!({"query":"alpha 专属经验"}));
    assert!(found.to_string().contains("alpha 专属经验"), "{found}");

    work::link_session(&conn, &task, "claude:alpha-run", "contribution", "user_confirmed").unwrap();
    let result_id: i64 = conn
        .query_row(
            "SELECT id FROM messages WHERE session_id='claude:alpha-run' AND kind='tool_result'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let verified = work::add_verification(&conn, &task, "test", "passed", Some(result_id), "", false).unwrap();
    assert_eq!(verified["result"], "passed");

    let pid: i64 = conn
        .query_row("SELECT project_id FROM tasks WHERE id=?1", [&task], |r| r.get(0))
        .unwrap();
    let page = work::timeline(&conn, Some(pid), None, None, None, &["message".into()], None, 1).unwrap();
    assert_eq!(page["has_more"], true);
    let older = work::timeline(&conn, Some(pid), None, None, None, &["message".into()], page["next_cursor"].as_str(), 1).unwrap();
    assert!(
        page["events"][0]["effective_sort"].as_f64() > older["events"][0]["effective_sort"].as_f64(),
        "首页是最新事件，游标向更早翻页：{page}\n{older}"
    );
}
