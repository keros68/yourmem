//! t4：ZCode adapter——rollout JSONL 解析、历史快照去重、杂行跳过、防御式、
//! 增量导入与历史行触发的全量重导（resync）。

use yourmem::adapters::{self, zcode};
use yourmem::{db, ingest};

fn line(query_source: &str, resp_text: &str, tool_calls: serde_json::Value) -> String {
    serde_json::json!({
        "type": "model_io",
        "querySource": query_source,
        "startedAt": "2026-08-23T08:00:01Z",
        "completedAt": "2026-08-23T08:00:09Z",
        "sessionId": "sess_test",
        "request": { "body": { "model": "test", "system": [{"type":"text","text":"sys"}] } },
        "response": { "text": resp_text, "toolCalls": tool_calls, "finishReason": "stop" }
    })
    .to_string()
}

#[test]
fn parses_turns_and_skips_title_and_bad_lines() {
    let unknown = serde_json::json!({
        "type": "future_event", "querySource": "whatever",
        "response": { "text": "未知类型的文本不该入账" }
    });
    let lines = vec![
        (1, line("session_title", "给会话起个名字", serde_json::json!([]))),
        (2, unknown.to_string()),
        (3, line("main_turn", "第一回合回复", serde_json::json!([]))),
        (4, line("main_turn", "", serde_json::json!([
            {"id": "c1", "name": "Bash", "input": {"command": "ls"}}
        ]))),
        (5, "{ 这是一行损坏的 JSON".to_string()),
        (6, line("main_turn", "第三回合", serde_json::json!([
            {"id": "c2", "name": "Write", "input": {"file_path": "/tmp/z/a.md", "content": "x"}}
        ]))),
    ];
    let out = zcode::parse_lines(&lines);
    let summary: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();
    assert_eq!(out.messages.len(), 4, "标题/未知类型/坏行不计: {summary:?}");
    assert!(summary.contains(&("assistant", "第一回合回复")));
    assert!(summary.contains(&("assistant", "第三回合")));
    assert!(summary.iter().any(|(k, c)| *k == "tool_call" && c.contains("[Bash]")));
    assert!(!summary.iter().any(|(_, c)| c.contains("起个名字")), "session_title 不得入账");
    assert!(!summary.iter().any(|(_, c)| c.contains("未知类型")), "非白名单 type 不得入账");
    assert_eq!(out.artifacts.len(), 1);
    assert_eq!(out.artifacts[0].path, "/tmp/z/a.md");
    // 时间窗：started 取 startedAt 最小，ended 取 completedAt 最大
    assert_eq!(out.meta.started_at.as_deref(), Some("2026-08-23T08:00:01Z"));
    assert_eq!(out.meta.ended_at.as_deref(), Some("2026-08-23T08:00:09Z"));
}

#[test]
fn history_snapshots_dedup_by_occurrence() {
    // 历史快照语义：同一快照内两条同文 user 是两条真消息（保留）；
    // 下一个快照里重复出现的是同一条（去重）；assistant 历史一律丢弃。
    // tool-result 真实格式是 role:"tool" 的 block 数组。
    let snap = |extra_user: &str| serde_json::json!({
        "type": "model_io", "querySource": "main_turn",
        "startedAt": "2026-08-23T08:10:00Z", "completedAt": "2026-08-23T08:10:05Z",
        "request": { "body": { "messages": [
            {"role": "user", "content": [{"type": "text", "text": "你好"}]},
            {"role": "user", "content": [{"type": "text", "text": "你好"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "历史里的旧回复"}]},
            {"role": "user", "content": [
                {"type": "image", "image": "x"},
                {"type": "text", "text": "带图的提问"}
            ]},
            {"role": "tool", "content": [{"type": "tool-result", "toolName": "Read",
                "output": {"type": "content", "value": "文件内容"}}]},
            {"role": "user", "content": [{"type": "text", "text": extra_user}]}
        ]}},
        "response": {"text": "", "toolCalls": []}
    });
    let lines = vec![
        (1, line("main_turn", "本回合回复", serde_json::json!([]))),
        (2, snap("快照一新增").to_string()),
        (3, snap("快照二新增").to_string()), // 与快照一重叠，只多一条
    ];
    let out = zcode::parse_lines(&lines);
    let summary: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();
    // 同快照两条"你好"都保留（真消息），跨快照不翻倍
    assert_eq!(out.messages.iter().filter(|m| m.content == "你好").count(), 2, "同快照双保留: {summary:?}");
    assert_eq!(out.messages.iter().filter(|m| m.content == "带图的提问").count(), 1);
    assert!(summary.contains(&("user", "快照一新增")));
    assert!(summary.contains(&("user", "快照二新增")));
    assert!(!summary.iter().any(|(_, c)| c.contains("历史里的旧回复")), "assistant 历史必须丢弃");
    assert!(summary.iter().any(|(k, c)| *k == "tool_result" && c.contains("[Read]") && c.contains("文件内容")));
    assert!(out.history_resync, "历史行应触发 resync 旗标");
}

#[test]
fn imports_rollout_file_incrementally() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("r");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("model-io-sess_00000000-0000-0000-0000-000000000001.jsonl");
    std::fs::write(&f, line("main_turn", "增量第一段", serde_json::json!([])) + "\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_ZCODE, dir.clone())];
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out.messages_added, 1);
    let cnt = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT COUNT(*) FROM messages WHERE session_id = 'zcode:sess_00000000-0000-0000-0000-000000000001'", [], |r| r.get(0)).unwrap()
    };
    assert_eq!(cnt(&conn), 1, "native_id 应剥掉 model-io- 前缀");

    // 追加一行再导：只入新增
    let mut f2 = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    use std::io::Write;
    writeln!(f2, "{}", line("main_turn", "增量第二段", serde_json::json!([]))).unwrap();
    drop(f2);
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out.messages_added, 1, "增量只入新行");
    assert_eq!(cnt(&conn), 2);

    // vault 归档行数对齐
    let archived: i64 = conn.query_row("SELECT COUNT(*) FROM vault_lines WHERE session_id = 'zcode:sess_00000000-0000-0000-0000-000000000001'", [], |r| r.get(0)).unwrap();
    assert_eq!(archived, 2, "每行原始字节都进 vault");
}

#[test]
fn history_line_triggers_full_resync_no_duplicates() {
    // 跨 chunk 场景：早前 chunk 已入账 response；新 chunk 出现历史行（含旧
    // 内容+新 user）→ 全量重导后无重复、新旧齐全（验收点完整闭环）。
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("r");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("model-io-sess_00000000-0000-0000-0000-000000000002.jsonl");
    std::fs::write(&f, line("main_turn", "回复一", serde_json::json!([])) + "\n"
        + &line("main_turn", "回复二", serde_json::json!([])) + "\n").unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_ZCODE, dir.clone())];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let cnt = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT COUNT(*) FROM messages WHERE session_id = 'zcode:sess_00000000-0000-0000-0000-000000000002'", [], |r| r.get(0)).unwrap()
    };
    assert_eq!(cnt(&conn), 2);

    // 追加历史行：包含重复的旧 assistant + 两条新 user
    let hist = serde_json::json!({
        "type": "model_io",
        "querySource": "main_turn",
        "startedAt": "2026-08-23T09:00:00Z",
        "completedAt": "2026-08-23T09:00:05Z",
        "request": {
            "body": {
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "用户问一"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "回复一"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "回复二"}]},
                    {"role": "user", "content": [{"type": "text", "text": "用户问二"}]}
                ]
            }
        },
        "response": {"text": "回复三", "toolCalls": []}
    });
    let mut f2 = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    use std::io::Write;
    writeln!(f2, "{hist}").unwrap();
    drop(f2);
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    // 最终：user×2 + response×3 = 5，无重复
    let contents: Vec<String> = {
        let mut stmt = conn.prepare("SELECT content FROM messages WHERE session_id = 'zcode:sess_00000000-0000-0000-0000-000000000002' ORDER BY content").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(contents.len(), 5, "resync 后应为 5 条: {contents:?}");
    let mut deduped = contents.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), 5, "不得有重复: {contents:?}");
    for expect in ["用户问一", "用户问二", "回复一", "回复二", "回复三"] {
        assert!(contents.iter().any(|c| c == expect), "缺 {expect}: {contents:?}");
    }
    // message_count 与 FTS 同步
    let mc: i64 = conn.query_row("SELECT message_count FROM sessions WHERE id = 'zcode:sess_00000000-0000-0000-0000-000000000002'", [], |r| r.get(0)).unwrap();
    assert_eq!(mc, 5);
    let fts: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '用户问'",
        [], |r| r.get(0)).unwrap_or(0);
    assert!(fts >= 2, "FTS 应索引新消息: {fts}");
}

#[test]
fn resync_corrects_inflated_counts_and_byte_fidelity() {
    // 场景一（负差）：模拟旧版本 bug 留下的虚高计数与重复消息——resync 修正
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("r");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("model-io-sess_00000000-0000-0000-0000-000000000003.jsonl");
    std::fs::write(&f, line("main_turn", "回复甲", serde_json::json!([])) + "\n").unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_ZCODE, dir.clone())];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    // 人为污染：虚高计数 + 重复消息行（旧行为的产物）
    conn.execute(
        "INSERT INTO messages(session_id, line_no, ord, kind, content) VALUES ('zcode:sess_00000000-0000-0000-0000-000000000003', 1, 1, 'assistant', '回复甲')",
        [],
    ).unwrap();
    conn.execute(
        "UPDATE sessions SET message_count = 9 WHERE id = 'zcode:sess_00000000-0000-0000-0000-000000000003'",
        [],
    ).unwrap();
    let hist = serde_json::json!({
        "type": "model_io",
        "querySource": "main_turn",
        "startedAt": "2026-08-23T09:00:00Z",
        "completedAt": "2026-08-23T09:00:05Z",
        "request": { "body": { "messages": [
            {"role": "user", "content": [{"type": "text", "text": "唯一的用户提问"}]}
        ]}},
        "response": {"text": "回复乙", "toolCalls": []}
    });
    let mut f2 = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    use std::io::Write;
    writeln!(f2, "{hist}").unwrap();
    drop(f2);
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let mc: i64 = conn.query_row("SELECT message_count FROM sessions WHERE id = 'zcode:sess_00000000-0000-0000-0000-000000000003'", [], |r| r.get(0)).unwrap();
    assert_eq!(mc, 3, "resync 后应为绝对值 3（甲/乙/用户提问）: {mc}");
    let cnt: i64 = conn.query_row("SELECT COUNT(*) FROM messages WHERE session_id = 'zcode:sess_00000000-0000-0000-0000-000000000003'", [], |r| r.get(0)).unwrap();
    assert_eq!(cnt, 3, "重复消息被清理");
    let net = out.messages_added;
    assert_eq!(net, 0, "净新增应为 0（3 - 旧污染计数 9 取 max 0），不虚报: {net}");

    // 场景二（字节保真）：CRLF 行 + 未换行尾部
    let dir2 = root.path().join("r2");
    std::fs::create_dir_all(&dir2).unwrap();
    let f3 = dir2.join("model-io-sess_00000000-0000-0000-0000-000000000004.jsonl");
    let body = line("main_turn", "CRLF 回复", serde_json::json!([])); // 行体本身无换行
    let partial_tail = "{\"type\":\"model_io\",\"querySource\":\"main_turn\""; // 无换行的半行
    std::fs::write(&f3, format!("{body}\r\n{partial_tail}")).unwrap(); // CRLF 行 + 未换行尾部
    let roots2 = vec![(adapters::AGENT_ZCODE, dir2.clone())];
    let out = ingest::import_all(&mut conn, home.path(), &roots2, None, None).unwrap();
    assert_eq!(out.lines_archived, 1, "只有完整行入档，未换行尾部留到下一轮");
    // vault 对象字节与原始 CRLF 行一致（含 \r）
    let (hash,): (String,) = conn.query_row(
        "SELECT hash FROM vault_lines WHERE session_id = 'zcode:sess_00000000-0000-0000-0000-000000000004' AND line_no = 1",
        [], |r| Ok((r.get(0)?,)),
    ).unwrap();
    let obj = home.path().join("objects").join(&hash[..2]).join(&hash);
    let raw = std::fs::read(&obj).unwrap();
    assert_eq!(raw, format!("{body}\r").as_bytes(), "vault 对象必须保留 \\r 字节");
    assert_eq!(raw.last(), Some(&b'\r'), "行尾 \\r 未被剥掉");
}

#[test]
fn compact_boundary_detected_min_wins_and_sliced() {
    // 0.3.8 压缩点：压缩后首个携带全量历史的请求，首条 user 消息是 harness 摘要
    // 前缀——该行即边界；多次压缩取最早（MIN），切片 = 边界之前的消息。
    let snap = |extra: &str| serde_json::json!({
        "type": "model_io", "querySource": "main_turn",
        "startedAt": "2026-08-23T08:20:00Z", "completedAt": "2026-08-23T08:20:05Z",
        "request": { "body": { "messages": [
            {"role": "user", "content": zcode::COMPACT_SUMMARY_PREFIX.to_string() + "……（摘要正文）"},
            {"role": "user", "content": [{"type": "text", "text": extra}]}
        ]}},
        "response": {"text": format!("压缩后回复：{extra}"), "toolCalls": []}
    });
    // adapter 级：chunk 内取最早（行 2），行 4 的第二次压缩不改写边界
    let lines = vec![
        (1, line("main_turn", "压缩前回复", serde_json::json!([]))),
        (2, snap("第一次压缩后提问").to_string()),
        (3, line("main_turn", "中间回合", serde_json::json!([]))),
        (4, snap("第二次压缩后提问").to_string()),
    ];
    let out = zcode::parse_lines(&lines);
    assert_eq!(out.compact_line, Some(2), "边界 = 摘要前缀最早出现的行");

    // ingest 级：增量导入 + 跨 chunk MIN + read_session 切片
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("r");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("model-io-sess_00000000-0000-0000-0000-000000000005.jsonl");
    std::fs::write(&f, line("main_turn", "压缩前回复", serde_json::json!([])) + "\n").unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_ZCODE, dir.clone())];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let sid = "zcode:sess_00000000-0000-0000-0000-000000000005";
    let cl: Option<i64> = conn
        .query_row("SELECT compact_line_no FROM sessions WHERE id = ?1", rusqlite::params![sid], |r| r.get(0))
        .unwrap();
    assert_eq!(cl, None, "未压缩前边界为 NULL");

    // 追加第一次压缩快照（行 2）→ 边界落 2；再追加第二次压缩快照（行 3）→ MIN 保持 2
    let mut f2 = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    use std::io::Write;
    writeln!(f2, "{}", snap("第一次压缩后提问")).unwrap();
    drop(f2);
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    // 行 3 是普通回合，行 4 是第二次压缩快照
    let mut f2 = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    writeln!(f2, "{}", line("main_turn", "中间回合", serde_json::json!([]))).unwrap();
    writeln!(f2, "{}", snap("第二次压缩后提问")).unwrap();
    drop(f2);
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    let cl: i64 = conn
        .query_row("SELECT compact_line_no FROM sessions WHERE id = ?1", rusqlite::params![sid], |r| r.get(0))
        .unwrap();
    assert_eq!(cl, 2, "多次压缩 MIN 保持首次边界");

    // 切片：before_compact 只含边界前消息（行 1 的 assistant），摘要/压缩后内容排除
    let full = db::read_session(&conn, sid, 100, None, false).unwrap();
    let pre = db::read_session(&conn, sid, 100, None, true).unwrap();
    assert_eq!(full["session"]["compact_line_no"], 2);
    assert_eq!(pre["before_compact"], true);
    let kinds: Vec<String> = pre["messages"].as_array().unwrap()
        .iter().map(|m| m["content"].as_str().unwrap_or("").to_string()).collect();
    assert!(kinds.iter().any(|c| c == "压缩前回复"), "边界前消息在切片内: {kinds:?}");
    assert!(!kinds.iter().any(|c| c.contains("压缩后")), "边界后消息必须排除: {kinds:?}");
    assert_eq!(pre["total_messages"], 1, "切片计数=边界前消息数");

    // markdown 导出：标题带压缩点行号，正文不含边界后内容
    let md = yourmem::dossier::render_session_markdown(&pre);
    assert!(md.contains("压缩前对话备份"), "标题缺失:\n{md}");
    assert!(md.contains("压缩点 行 2"), "压缩点注记缺失:\n{md}");
    assert!(!md.contains("压缩后"), "导出不得混入边界后内容:\n{md}");
}

#[test]
fn migrate_v4_dedup_remaps_pointers_and_recounts() {
    // codex 五审复现场景：v4 库带重复消息 + memory 指向将被删的重复行。
    // 迁移必须：能打开（索引建在去重后）、指针重定向到保留行、计数重算。
    let home = tempfile::tempdir().unwrap();
    {
        let conn = rusqlite::Connection::open(home.path().join("yourmem.db")).unwrap();
        // 手工搭一个 v4 形态的库（SCHEMA 建表后回退版本、制造重复）
        let c2 = yourmem::db::open(home.path()).unwrap(); // 正常建库（此时已 v5）
        drop(c2);
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute_batch("DROP INDEX IF EXISTS idx_messages_slo;").unwrap();
        // 会话 + 两组同键消息（rowid 小的保留）
        conn.execute_batch(
            "INSERT INTO sessions(id, agent, native_id, file_path, message_count, created_at, updated_at)
             VALUES ('claude:mig', 'claude', 'mig', '/tmp/mig.jsonl', 2, '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z');
             INSERT INTO messages(session_id, line_no, ord, kind, content)
             VALUES ('claude:mig', 1, 0, 'user', '重复消息');
             INSERT INTO messages(session_id, line_no, ord, kind, content)
             VALUES ('claude:mig', 1, 0, 'user', '重复消息');",
        )
        .unwrap();
        let dup_rowid: i64 = conn.query_row(
            "SELECT MAX(rowid) FROM messages WHERE session_id = 'claude:mig'", [], |r| r.get(0),
        ).unwrap();
        conn.execute(
            "INSERT INTO memories(id, scope, type, content, status, source_session_id, source_message_id, created_at, updated_at)
             VALUES ('mem_mig', 'global', 'fact', '指向重复行的记忆', 'confirmed', 'claude:mig', ?1, '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')",
            rusqlite::params![dup_rowid],
        ).unwrap();
    }
    // 打开触发迁移（此前会在 SCHEMA 建唯一索引处直接炸——五审 blocker 1）
    let dup_rowid: i64 = {
        let c = rusqlite::Connection::open(home.path().join("yourmem.db")).unwrap();
        c.query_row("SELECT MAX(rowid) FROM messages WHERE session_id = 'claude:mig'", [], |r| r.get(0)).unwrap()
    };
    let conn = yourmem::db::open(home.path()).unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, yourmem::db::SCHEMA_VERSION as i64, "脏库一路迁到当前版本");
    let msgs: i64 = conn.query_row("SELECT COUNT(*) FROM messages WHERE session_id = 'claude:mig'", [], |r| r.get(0)).unwrap();
    assert_eq!(msgs, 1, "重复被去");
    let keep: i64 = conn.query_row("SELECT MIN(rowid) FROM messages WHERE session_id = 'claude:mig'", [], |r| r.get(0)).unwrap();
    let pointed: i64 = conn.query_row("SELECT source_message_id FROM memories WHERE id = 'mem_mig'", [], |r| r.get(0)).unwrap();
    assert_eq!(pointed, keep, "指针必须重定向到保留行（原指向 {dup_rowid}，保留 {keep}）");
    let cnt: i64 = conn.query_row("SELECT message_count FROM sessions WHERE id = 'claude:mig'", [], |r| r.get(0)).unwrap();
    assert_eq!(cnt, 1, "计数重算");
    let fts: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '重复消息'", [], |r| r.get(0),
    ).unwrap_or(0);
    assert_eq!(fts, 1, "FTS 触发器联动，无残留");
    let idx: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'idx_messages_slo'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(idx, 1, "唯一索引已建");
}
