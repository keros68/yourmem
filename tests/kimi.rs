//! t5：Kimi adapter——wire 事件流解析、turn.prompt/steer 权威用户消息、
//! append_message 去重跳过、think(part.think)/text 分流、args.path artifact、
//! 防御式、路径派生身份、state.json cwd 补查与迟到回填。
//! 主解析 fixture（tests/fixtures/kimi/）是全合成样本：事件结构按真机 wire 协议
//! 逐型保真，内容不含真实会话（隐私纪律；adapter 本体开发时已用真机样本验证）。

use yourmem::adapters::{self, kimi};
use yourmem::{db, ingest};

#[allow(dead_code)]
fn ev(event: serde_json::Value, time: i64) -> String {
    serde_json::json!({ "type": "context.append_loop_event", "agentId": "main", "event": event, "time": time }).to_string()
}

#[test]
fn parses_sample_fixture() {
    // 合成样本（12 行覆盖全部事件类型）
    let raw = std::fs::read_to_string("tests/fixtures/kimi/wire.jsonl").unwrap();
    let lines: Vec<(u64, String)> = raw.lines().enumerate().map(|(i, l)| ((i + 1) as u64, l.to_string())).collect();
    let out = kimi::parse_lines(&lines);
    let summary: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();

    // 用户输入：turn.prompt + turn.steer（steer 走 skill_activation 形态）
    let users = out.messages.iter().filter(|m| m.kind.as_str() == "user").count();
    assert_eq!(users, 2, "prompt + steer 各一条: {summary:?}");
    // append_message 与 turn.prompt 同文——不得双录
    assert_eq!(out.messages.iter().filter(|m| m.content.contains("开工 示例迭代")).count(), 1, "append_message 副本必须跳过");
    // think → Thinking（真实字段 part.think）
    assert!(summary.iter().any(|(k, c)| *k == "thinking" && !c.is_empty()), "think 必须用 part.think: {summary:?}");
    // text → Assistant
    assert!(summary.iter().any(|(k, _)| *k == "assistant"));
    // tool.call（Bash/Write）与 tool.result
    assert!(summary.iter().any(|(k, c)| *k == "tool_call" && c.contains("[Bash]")));
    assert!(summary.iter().any(|(k, c)| *k == "tool_call" && c.contains("[Write]")));
    assert!(summary.iter().any(|(k, _)| *k == "tool_result"));
    // Write 的 artifact 用参数字段 args.path
    assert_eq!(out.artifacts.len(), 1, "样本恰有一个 Write: {:?}", out.artifacts);
    assert!(out.artifacts[0].path.ends_with(".rs"), "args.path: {}", out.artifacts[0].path);
    assert_eq!(out.artifacts[0].tool, "Write");
    // 时间窗来自 epoch 毫秒（2026 合成值）
    assert!(out.meta.started_at.as_deref().unwrap_or("").starts_with("2026-"), "{:?}", out.meta.started_at);
}

#[test]
fn edge_cases_bad_lines_and_unknown_events() {
    let lines = vec![
        (1, "{ 损坏行".to_string()),
        (2, r#"{"type":"future_outer_type","response":{"text":"未知外层不该入账"}}"#.to_string()),
        (3, serde_json::json!({"type":"context.append_loop_event","time": 99999999999999999i64,
            "event":{"type":"content.part","part":{"type":"future_part","x":1}}}).to_string()),
        (4, serde_json::json!({"type":"context.append_loop_event","time": 1787470000000i64,
            "event":{"type":"content.part","part":{"type":"text","text":"有效回复"}}}).to_string()),
    ];
    let out = kimi::parse_lines(&lines);
    let summary: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();
    assert_eq!(out.messages.len(), 1, "{summary:?}");
    assert_eq!(summary[0], ("assistant", "有效回复"));
    assert!(!summary.iter().any(|(_, c)| c.contains("未知外层")), "未知外层类型跳过");
    // 越界毫秒不产生时间戳（按缺失降级），有效行时间正常
    assert_eq!(out.meta.started_at.as_deref(), Some("2026-08-23T07:26:40.000Z"));
}

#[test]
fn imports_session_dir_with_state_json_cwd() {
    // 目录结构 …/sessions/wd_x/session_<uuid>/agents/main/wire.jsonl + state.json
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let sess_dir = root.path().join("wd_abc123").join("session_00000000-0000-0000-0000-00000000000a");
    std::fs::create_dir_all(sess_dir.join("agents").join("main")).unwrap();
    std::fs::write(sess_dir.join("state.json"), r#"{"id":"session_00000000-0000-0000-0000-00000000000a","cwd":"/Users/test/myproj","version":2}"#).unwrap();
    let wire = serde_json::json!({"type":"turn.prompt","agentId":"main","time":1787470000000i64,
        "input":[{"type":"text","text":"路径派生身份测试"}]});
    std::fs::write(sess_dir.join("agents").join("main").join("wire.jsonl"), wire.to_string() + "\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_KIMI, root.path().to_path_buf())];
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out.messages_added, 1);
    let (cwd, pid_path): (Option<String>, Option<String>) = conn.query_row(
        "SELECT s.cwd, p.path FROM sessions s LEFT JOIN projects p ON p.id = s.project_id WHERE s.id = 'kimi:session_00000000-0000-0000-0000-00000000000a'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(cwd.as_deref(), Some("/Users/test/myproj"), "cwd 从 state.json 补查");
    assert!(pid_path.is_some(), "有 cwd 就应归属项目");
    // resume 命令
    assert_eq!(
        adapters::resume_command("kimi", "session_00000000-0000-0000-0000-00000000000a").as_deref(),
        Some("kimi -S session_00000000-0000-0000-0000-00000000000a")
    );
    // 子 agent 会话（id 含 #）：恢复命令不提供
    assert!(adapters::resume_command("kimi", "session_x#sub1").is_none());
}

#[test]
fn subagent_wire_is_separate_session() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let sess_dir = root.path().join("wd_h").join("session_00000000-0000-0000-0000-00000000000b");
    for agent_name in ["main", "researcher"] {
        std::fs::create_dir_all(sess_dir.join("agents").join(agent_name)).unwrap();
        let wire = serde_json::json!({"type":"context.append_loop_event","agentId":agent_name,"time":1787470000000i64,
            "event":{"type":"content.part","part":{"type":"text","text":format!("{agent_name} 的输出")}}});
        std::fs::write(sess_dir.join("agents").join(agent_name).join("wire.jsonl"), wire.to_string() + "\n").unwrap();
    }
    std::fs::write(sess_dir.join("state.json"), r#"{"cwd":"/tmp/p"}"#).unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_KIMI, root.path().to_path_buf())];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM sessions WHERE id LIKE 'kimi:session_00000000-0000-0000-0000-00000000000b%' ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(ids, vec![
        "kimi:session_00000000-0000-0000-0000-00000000000b".to_string(),
        "kimi:session_00000000-0000-0000-0000-00000000000b#researcher".to_string(),
    ], "main 本体 + 子 agent 独立成会话");
}

#[test]
fn late_state_json_backfills_cwd_and_artifact_project() {
    // 两阶段（codex 二审点名的测试缺口）：先无 state 导入 → 补 state →
    // 无新增字节的再导入触发迟到回填。断言会话与 artifact 都归属项目，
    // 且旁边放了同名 state.json 的 claude 会话不受影响。
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let kdir = root.path().join("wd_k").join("session_00000000-0000-0000-0000-00000000000c");
    std::fs::create_dir_all(kdir.join("agents").join("main")).unwrap();
    let wire = serde_json::json!({
        "type": "context.append_loop_event", "agentId": "main", "time": 1787470000000i64,
        "event": {"type": "tool.call", "name": "Write", "args": {"path": "src/late.md", "content": "x"}}
    });
    std::fs::write(kdir.join("agents").join("main").join("wire.jsonl"), wire.to_string() + "\n").unwrap();
    // 干扰项（真防线，codex 三审指出此前是假的）：claude fixture 无 cwd，
    // 且 state.json 放在其 nth(3) 祖先——若回填没按 agent 把关，它会被
    // kimi 的 state 逻辑错挂到 youmem 项目。
    let cdir = root.path().join("dep1").join("dep2").join("dep3");
    std::fs::create_dir_all(&cdir).unwrap();
    std::fs::write(cdir.join("cccc.jsonl"), format!("{{\"type\":\"user\",\"uuid\":\"c1\",\"timestamp\":\"2026-08-23T10:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"无 cwd 的干扰会话\"}}}}\n")).unwrap();
    // ancestors() 含自身：cccc.jsonl→dep3→dep2→dep1→root，nth(3)=dep1——
    // state.json 必须放 dep1，未加 agent 门控的旧逻辑才真能读到（codex 四审）
    std::fs::write(root.path().join("dep1").join("state.json"), r#"{"cwd":"/Users/devuser/youmem"}"#).unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![
        (adapters::AGENT_KIMI, root.path().join("wd_k")),
        (adapters::AGENT_CLAUDE, root.path().join("dep1").join("dep2").join("dep3")),
    ];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let (cwd, pid): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT cwd, project_id FROM sessions WHERE id = 'kimi:session_00000000-0000-0000-0000-00000000000c'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert!(cwd.is_none() && pid.is_none(), "首次无 state：无归属");
    let art_pid: Option<i64> = conn.query_row(
        "SELECT project_id FROM session_artifacts WHERE session_id = 'kimi:session_00000000-0000-0000-0000-00000000000c'",
        [], |r| r.get(0),
    ).unwrap();
    assert!(art_pid.is_none());

    // 补 state.json → 无新增字节的再导入 → 迟到回填
    std::fs::write(kdir.join("state.json"), r#"{"cwd":"/Users/devuser/youmem"}"#).unwrap();
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let (cwd, pid): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT cwd, project_id FROM sessions WHERE id = 'kimi:session_00000000-0000-0000-0000-00000000000c'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(cwd.as_deref(), Some("/Users/devuser/youmem"), "迟到回填 cwd");
    assert!(pid.is_some(), "迟到回填项目归属");
    let art_pid: i64 = conn.query_row(
        "SELECT project_id FROM session_artifacts WHERE session_id = 'kimi:session_00000000-0000-0000-0000-00000000000c'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(art_pid, pid.unwrap(), "artifact 与会话同项目（按 artifact 自身 project_id 才能查到）");
    // 干扰项：claude 会话（无 cwd）不得被 kimi 的 state.json 越权认领
    let cl_pid: Option<i64> = conn.query_row(
        "SELECT project_id FROM sessions WHERE id = 'claude:cccc'", [], |r| r.get(0),
    ).unwrap();
    let cl_cwd: Option<String> = conn.query_row(
        "SELECT cwd FROM sessions WHERE id = 'claude:cccc'", [], |r| r.get(0),
    ).unwrap();
    assert!(cl_pid.is_none() && cl_cwd.is_none(), "claude 会话不得被 kimi 回填认领: pid={cl_pid:?} cwd={cl_cwd:?}");
}

#[test]
fn state_arrives_with_new_bytes_repairs_old_artifact_project() {
    // codex 三审场景：首次无 state 导入 artifact（project NULL）→ state 到位
    // 且 wire.jsonl 同时追加新行（走增量路径而非回填分支）→ 旧 artifact 的
    // 空归属必须被同事务的无条件修复补上。
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let kdir = root.path().join("session_00000000-0000-0000-0000-00000000000d");
    std::fs::create_dir_all(kdir.join("agents").join("main")).unwrap();
    let wire_path = kdir.join("agents").join("main").join("wire.jsonl");
    let w1 = serde_json::json!({
        "type": "context.append_loop_event", "agentId": "main", "time": 1787470000000i64,
        "event": {"type": "tool.call", "name": "Write", "args": {"path": "src/old.md", "content": "x"}}
    });
    std::fs::write(&wire_path, w1.to_string() + "\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_KIMI, root.path().to_path_buf())];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let art_pid: Option<i64> = conn.query_row(
        "SELECT project_id FROM session_artifacts WHERE session_id = 'kimi:session_00000000-0000-0000-0000-00000000000d'",
        [], |r| r.get(0),
    ).unwrap();
    assert!(art_pid.is_none(), "首次无 state：artifact 无归属");

    // state 到位 + 追加新行（增量路径）
    std::fs::write(kdir.join("state.json"), r#"{"cwd":"/Users/devuser/youmem"}"#).unwrap();
    let w2 = serde_json::json!({
        "type": "context.append_loop_event", "agentId": "main", "time": 1787470001000i64,
        "event": {"type": "content.part", "part": {"type": "text", "text": "新回复"}}
    });
    let mut f = std::fs::OpenOptions::new().append(true).open(&wire_path).unwrap();
    use std::io::Write;
    writeln!(f, "{w2}").unwrap();
    drop(f);
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    let (spid, apid): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT s.project_id, (SELECT a.project_id FROM session_artifacts a WHERE a.session_id = s.id LIMIT 1)
         FROM sessions s WHERE s.id = 'kimi:session_00000000-0000-0000-0000-00000000000d'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert!(spid.is_some(), "增量路径会话获得归属");
    assert_eq!(apid, spid, "旧 artifact 的空归属被增量路径修复（codex 三审）");
}

#[test]
fn backfill_is_atomic_and_retryable() {
    // 原子性证明（codex 四审建议）：用 trigger 让 artifact 更新中途 RAISE，
    // 断言导入报错且 session 归属未写入（事务回滚）；移除 trigger 重试成功。
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let kdir = root.path().join("session_00000000-0000-0000-0000-00000000000e");
    std::fs::create_dir_all(kdir.join("agents").join("main")).unwrap();
    let wire = serde_json::json!({
        "type": "context.append_loop_event", "agentId": "main", "time": 1787470000000i64,
        "event": {"type": "tool.call", "name": "Write", "args": {"path": "src/atomic.md", "content": "x"}}
    });
    std::fs::write(kdir.join("agents").join("main").join("wire.jsonl"), wire.to_string() + "\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_KIMI, root.path().to_path_buf())];
    // 第一次：无 state 导入（artifact 无归属）
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    // state 到位 + artifact 更新被 trigger 拒绝 → 无新字节的回填路径应整体失败
    std::fs::write(kdir.join("state.json"), r#"{"cwd":"/Users/devuser/youmem"}"#).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_artifact_update BEFORE UPDATE ON session_artifacts
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    ).unwrap();
    let r = ingest::import_all(&mut conn, home.path(), &roots, None, None);
    assert!(r.is_err(), "artifact 更新失败必须让导入报错");
    let (cwd, pid): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT cwd, project_id FROM sessions WHERE id = 'kimi:session_00000000-0000-0000-0000-00000000000e'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert!(cwd.is_none() && pid.is_none(), "事务回滚：session 不得半提交（cwd={cwd:?} pid={pid:?}）");
    // 第二次：移除 trigger，无新字节路径重试成功
    conn.execute_batch("DROP TRIGGER fail_artifact_update").unwrap();
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let (cwd, pid): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT cwd, project_id FROM sessions WHERE id = 'kimi:session_00000000-0000-0000-0000-00000000000e'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert!(cwd.is_some() && pid.is_some(), "失败后可重试自愈");
}
