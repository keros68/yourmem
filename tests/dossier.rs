//! t3（§7.2/7.3）：项目卷宗聚合、决策 superseded 演变链、markdown 导出、日报卡。
//! 评审加固后语义（codex 9 blocker）：决策板只展示 confirmed；演变链环防御；
//! 来源指针带消息行号；artifacts 全量；digest 含决策数/open tasks/最近 handoff。

use yourmem::{adapters, db, dossier, ingest};

const S1: &str = r#"{"type":"user","cwd":"/tmp/dos-proj","uuid":"d1","timestamp":"2026-08-23T09:00:00Z","message":{"role":"user","content":"卷宗项目的第一条"}}
{"type":"assistant","uuid":"d2","timestamp":"2026-08-23T09:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"卷宗回复一"}]}}"#;

const S2: &str = r#"{"type":"user","cwd":"/tmp/dos-proj","uuid":"d3","timestamp":"2026-08-23T10:00:00Z","message":{"role":"user","content":"卷宗项目的第二条"}}
{"type":"assistant","uuid":"d4","timestamp":"2026-08-23T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"卷宗回复二"}]}}"#;

fn fixture() -> (tempfile::TempDir, i64) {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dir = src.path().join("claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("dos1.jsonl"), format!("{S1}\n")).unwrap();
    std::fs::write(dir.join("dos2.jsonl"), format!("{S2}\n")).unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, dir)];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    let pid: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE path = '/tmp/dos-proj'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // 决策链：v1 被 v2 取代，v2 被 v3 取代；v3 确认（confirm 清 superseded_by）。
    // v3 带消息级来源指针（source_message_id → 行号）。
    let first_msg: i64 = conn
        .query_row(
            "SELECT id FROM messages WHERE session_id = 'claude:dos1' ORDER BY line_no LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let second_msg: i64 = conn
        .query_row("SELECT id FROM messages WHERE session_id = 'claude:dos1' ORDER BY line_no LIMIT 1 OFFSET 1", [], |r| r.get(0))
        .unwrap();
    // 直插固定 created_at：digest 按日聚合断言 2026-08-23，走 save_memory 的真实
    // 时钟会把测试变成日期炸弹（交付次日必红）
    let mem = |content: &str, msg: Option<i64>, id: &str| {
        conn.execute(
            "INSERT INTO memories(id, project_id, scope, type, content, status,
                 source_session_id, source_message_id, created_at, updated_at)
             VALUES (?1, ?2, 'project', 'decision', ?3, 'suggested', 'claude:dos1', ?4,
                 '2026-08-23T11:00:00Z', '2026-08-23T11:00:00Z')",
            rusqlite::params![id, pid, content, msg],
        )
        .unwrap();
    };
    mem("用 SQLite 存会话（v1）", Some(first_msg), "mem_v1");
    mem(
        "用 SQLite + FTS5 + vault 备份的 v2",
        Some(second_msg),
        "mem_v2",
    );
    mem(
        "用 SQLite + FTS5 + vault 备份（v3）",
        Some(first_msg),
        "mem_v3",
    );
    db::update_memory_status(&conn, "mem_v1", "supersede", Some("mem_v2")).unwrap();
    db::update_memory_status(&conn, "mem_v2", "supersede", Some("mem_v3")).unwrap();
    db::update_memory_status(&conn, "mem_v3", "confirm", None).unwrap();

    conn.execute(
        "INSERT INTO session_artifacts(session_id, project_id, path, tool, created_at)
         VALUES ('claude:dos1', ?1, '/tmp/dos-proj/report.md', 'Write', '2026-08-23T09:05:00Z')",
        rusqlite::params![pid],
    )
    .unwrap();
    db::create_handoff(
        &conn,
        pid,
        &db::HandoffFields {
            title: "卷宗功能动工",
            done: "dossier 模块",
            state: "进行中",
            decisions: "聚合全走 SQL",
            files_changed: "src/dossier.rs",
            open_issues: "",
            next_steps: "接 UI",
            session_id: Some("claude:dos2"),
        },
    )
    .unwrap();
    (home, pid)
}

#[test]
fn dossier_aggregates_and_renders_markdown_with_chains() {
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();

    let d = dossier::project_dossier(&conn, pid).unwrap();
    assert_eq!(d["overview"]["sessions"], 2);
    assert_eq!(d["overview"]["messages"], 4);
    assert_eq!(d["decisions"].as_array().unwrap().len(), 3);
    assert_eq!(d["timeline"].as_array().unwrap().len(), 2);
    assert_eq!(d["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(d["handoffs"].as_array().unwrap().len(), 1);

    // 软删隐藏：卷宗概览与时间线与搜索同口径
    db::set_session_deleted(&conn, "claude:dos1", true).unwrap();
    let d2 = dossier::project_dossier(&conn, pid).unwrap();
    assert_eq!(d2["overview"]["sessions"], 1);
    assert_eq!(d2["timeline"].as_array().unwrap().len(), 1);
    db::set_session_deleted(&conn, "claude:dos1", false).unwrap();

    // markdown：confirmed 带头 / 两级演变链 / 会话+行号来源指针 / artifact / handoff
    let md = dossier::render_markdown(&d);
    assert!(md.contains("# 项目卷宗：dos-proj"), "标题缺失:\n{md}");
    assert!(
        md.contains("vault 备份（v3）"),
        "活跃（confirmed）决策缺失:\n{md}"
    );
    let chain_count = md.matches("演变史").count();
    assert!(
        chain_count >= 2,
        "演变链应有两级，实际 {chain_count}:\n{md}"
    );
    assert!(md.contains("SQLite 存会话（v1）"), "链尾 v1 缺失:\n{md}");
    assert!(
        md.contains("`claude:dos1#L1`"),
        "决策来源应带行号指针:\n{md}"
    );
    let hist_line = md
        .lines()
        .find(|l| l.contains("演变史") && l.contains("v1"))
        .unwrap_or_else(|| panic!("演变史应包含 v1 且带行号:\n{md}"));
    assert!(
        hist_line.contains("#L1"),
        "历史决策也要有行号指针:\n{hist_line}"
    );
    let hist2 = md
        .lines()
        .find(|l| l.contains("演变史") && l.contains("v2"))
        .unwrap_or_else(|| panic!("演变史应包含 v2:\n{md}"));
    assert!(
        hist2.contains("#L2"),
        "v2 必须用自己的 L2 来源（不得复用链头）:\n{hist2}"
    );
    assert!(
        md.contains("`claude:dos2`"),
        "时间线/handoff 来源指针缺失:\n{md}"
    );
    assert!(md.contains("report.md"), "artifact 缺失:\n{md}");
    assert!(md.contains("卷宗功能动工"), "handoff 缺失:\n{md}");
    // suggested 的决策不该出现在板里（fixture 的 v3 已 confirm；再造一条 suggested 验证）
    db::save_memory(
        &conn,
        &db::MemoryInput {
            project_id: Some(pid),
            scope: "project",
            r#type: "decision",
            content: "待确认的草稿决策",
            status: None,
            source_session_id: None,
            source_message_id: None,
        },
    )
    .unwrap();
    let d3 = dossier::project_dossier(&conn, pid).unwrap();
    let md3 = dossier::render_markdown(&d3);
    assert!(
        !md3.contains("待确认的草稿决策"),
        "suggested 决策不得入板:\n{md3}"
    );
    // 数据契约层就要排除（CLI JSON / MCP 同源），不只是渲染层过滤
    assert_eq!(
        d3["decisions"].as_array().unwrap().len(),
        3,
        "suggested 不得进 decisions JSON"
    );
    for m in d3["decisions"].as_array().unwrap() {
        assert!(matches!(
            m["status"].as_str(),
            Some("confirmed") | Some("superseded")
        ));
    }
}

#[test]
fn decision_chain_cycle_terminates() {
    // 数据层 confirm 已清 superseded_by，但老库可能已有环——渲染必须防御性截断。
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    let head: String = conn
        .query_row(
            "SELECT id FROM memories WHERE content LIKE '%v3%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let p2: String = conn
        .query_row(
            "SELECT id FROM memories WHERE content LIKE '%v2%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // 存量环构造（老库在 confirm-清链修复前可能留下）：confirmed 链头 v3 的
    // superseded_by 仍指 v2，v2 又指回 v3 —— 反向遍历 v3→v2→v3 必须被截断。
    conn.execute(
        "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
        rusqlite::params![p2, head],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
        rusqlite::params![head, p2],
    )
    .unwrap();
    // 必须在合理时间内终止（环存在时不 hang）
    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    assert!(md.contains("vault 备份（v3）"), "链头仍应渲染:\n{md}");
    assert!(
        md.matches("演变史").count() <= 2,
        "环截断后演变史不应失控:\n{md}"
    );
    // Mermaid 图只画直接 superseded_by 边、不沿链遍历，环数据照样有限输出
    assert!(
        md.contains("```mermaid") && md.contains("graph LR"),
        "环数据也应出图:\n{md}"
    );
    assert_eq!(
        md.matches("|superseded|").count(),
        3,
        "环 = 三条直接边（v1→v2→v3→v2）:\n{md}"
    );
}

#[test]
fn markdown_decision_chain_mermaid_is_deterministic_and_labeled() {
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    let g = &md[md.find("### 决策演变链").unwrap()..];
    // decs 按 (created_at, id) 排序 → m0=v1, m1=v2, m2=v3；边只沿 superseded_by
    assert!(g.contains("```mermaid\ngraph LR"), "图块缺失:\n{md}");
    assert!(
        g.contains("m0[\"用 SQLite 存会话（v1）\"]"),
        "节点标签缺失:\n{g}"
    );
    assert!(g.contains("m0 -->|superseded| m1"), "演变边缺失:\n{g}");
    assert!(g.contains("m1 -->|superseded| m2"), "演变边缺失:\n{g}");
    assert!(g.contains("class m0,m1 old"), "superseded 灰显缺失:\n{g}");
    // 演变链图挂在决策板章节内，不越界到时间线
    assert!(
        g.find("graph LR").unwrap() < g.find("## 时间线").unwrap(),
        "图应位于决策板内:\n{g}"
    );
}

#[test]
fn markdown_lineage_mermaid_draws_links_and_external_parents() {
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    conn.execute(
        "INSERT INTO session_links(child_session_id, parent_session_id, link_type, via_uuid, created_at)
         VALUES ('claude:dos2', 'claude:dos1', 'fork', NULL, '2026-08-23T10:00:00Z')",
        [],
    )
    .unwrap();
    // 跨项目父：claude:ghost 不在本项目时间线，应入图且标签退化为会话 id
    conn.execute(
        "INSERT INTO session_links(child_session_id, parent_session_id, link_type, via_uuid, created_at)
         VALUES ('claude:dos1', 'claude:ghost', 'compact', NULL, '2026-08-23T09:30:00Z')",
        [],
    )
    .unwrap();
    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    let g = &md[md.find("### 会话谱系").unwrap()..];
    // 时间线 ORDER BY started_at：dos1(09:00) 行先入边 → ghost=s0, dos1=s1, dos2=s2
    assert!(
        g.contains("s0[\"claude:ghost\"]"),
        "外部父节点标签应退化为 id:\n{g}"
    );
    assert!(
        g.contains("s1[\"claude 2026-08-23 09:00\"]"),
        "节点标签=agent+起始时间:\n{g}"
    );
    assert!(g.contains("s0 -->|compact| s1"), "compact 边缺失:\n{g}");
    assert!(g.contains("s1 -->|fork| s2"), "fork 边缺失:\n{g}");
}

#[test]
fn markdown_mermaid_labels_are_sanitized() {
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    // 自由文本：引号/方括号/换行+行首 ## / 井号实体——任何一个漏进图代码都炸渲染
    let dirty = "他说 \"hi\" [选项A]\n## 注入 #x;";
    conn.execute(
        "INSERT INTO memories(id, project_id, scope, type, content, status, superseded_by,
             source_session_id, created_at, updated_at)
         VALUES ('mem_dirty', ?1, 'project', 'decision', ?2, 'superseded', 'mem_v3',
             'claude:dos1', '2026-08-23T11:30:00Z', '2026-08-23T11:30:00Z')",
        rusqlite::params![pid, dirty],
    )
    .unwrap();
    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    // 只切决策演变链第一个图块（文档里可能还有谱系图）
    let start = md.find("```mermaid").unwrap();
    let end = start + md[start..].find("\n```").unwrap();
    let g = &md[start..end];
    // 清洗后：空白折叠、危险字符全部移除（#x; 里的 x 保留）
    assert!(
        g.contains("m3[\"他说 hi 选项A 注入 x\"]"),
        "标签应被清洗:\n{g}"
    );
    // 裸引号只允许做标签定界：总引号数 = 节点数 × 2
    assert_eq!(
        g.matches('"').count(),
        g.matches("[\"").count() * 2,
        "裸引号只允许做标签定界:\n{g}"
    );
    assert!(!g.contains("##"), "行首 ## 注入必须被清洗:\n{g}");
    assert!(!g.contains("#x;"), "实体式 #x; 必须被清洗:\n{g}");
    // decs 顺序 v1,v2,v3(11:00),dirty(11:30) → m3；边 m3→m2
    assert!(g.contains("m3 -->|superseded| m2"), "脏节点也上链:\n{g}");
}

#[test]
fn markdown_omits_mermaid_when_no_chains_or_links() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    conn.execute(
        "INSERT INTO projects(name, path, created_at, updated_at)
         VALUES ('empty', '/tmp/empty-proj', '2026-08-23T09:00:00Z', '2026-08-23T09:00:00Z')",
        [],
    )
    .unwrap();
    let pid: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE path = '/tmp/empty-proj'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    assert!(!md.contains("mermaid"), "无链无谱系时不得出图:\n{md}");
}

#[test]
fn confirm_clears_nonempty_superseded_by_and_join_guards_session() {
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    // 先给目标挂上非空 superseded_by，再 confirm，断言清空
    let v3: String = conn
        .query_row(
            "SELECT id FROM memories WHERE content LIKE '%v3%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let v1: String = conn
        .query_row(
            "SELECT id FROM memories WHERE content LIKE '%v1%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(db::update_memory_status(&conn, &v3, "supersede", Some(&v1)).is_err());
    // 模拟升级前遗留的环；现在的写接口拒绝创建，confirm 仍须能修复。
    conn.execute(
        "UPDATE memories SET status='superseded', superseded_by=?1 WHERE id=?2",
        rusqlite::params![v1, v3],
    )
    .unwrap();
    let sb: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE id = ?1",
            rusqlite::params![v3],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sb.as_deref(), Some(v1.as_str()));
    db::update_memory_status(&conn, &v3, "confirm", None).unwrap();
    let sb: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE id = ?1",
            rusqlite::params![v3],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sb.is_none(), "confirm 必须清掉非空 superseded_by");

    // 跨会话行号防错挂：老库脏数据（message 属 dos1 却标 dos2）不得伪造行号指针
    let dos1_msg: i64 = conn
        .query_row(
            "SELECT id FROM messages WHERE session_id = 'claude:dos1' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO memories(id, project_id, scope, type, content, status,
                              source_session_id, source_message_id, created_at, updated_at)
         VALUES ('mem_dirty', ?1, 'project', 'decision', '脏来源决策', 'confirmed',
                 'claude:dos2', ?2, '2026-08-23T11:00:00Z', '2026-08-23T11:00:00Z')",
        rusqlite::params![pid, dos1_msg],
    )
    .unwrap();
    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    let dirty_line = md
        .lines()
        .find(|l| l.contains("脏来源决策"))
        .unwrap_or_else(|| panic!("该决策本身应仍可见:\n{md}"));
    assert!(
        !dirty_line.contains("#L"),
        "跨会话 message 不得伪造行号:\n{dirty_line}"
    );

    // 写入侧：API 拒绝不属于所指会话的 source_message_id
    let bad = db::save_memory(
        &conn,
        &db::MemoryInput {
            project_id: Some(pid),
            scope: "project",
            r#type: "decision",
            content: "又一条脏的",
            status: None,
            source_session_id: Some("claude:dos2"),
            source_message_id: Some(dos1_msg),
        },
    );
    assert!(bad.is_err(), "写入侧必须拒绝跨会话 message id");
}

#[test]
fn dossier_returns_all_artifacts_without_cap() {
    // §7.2 要求"全部"写文件产物——250 个也要全回
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    for i in 0..250 {
        conn.execute(
            "INSERT INTO session_artifacts(session_id, project_id, path, tool, created_at)
             VALUES ('claude:dos2', ?1, ?2, 'Write', '2026-08-23T10:30:00Z')",
            rusqlite::params![pid, format!("/tmp/dos-proj/out/{i:03}.md")],
        )
        .unwrap();
    }
    let d = dossier::project_dossier(&conn, pid).unwrap();
    assert_eq!(d["artifacts"].as_array().unwrap().len(), 251); // 250 + fixture 的 1 个
    let md = dossier::render_markdown(&d);
    assert!(
        md.contains("全部 251 项"),
        "markdown 应声明全量:\n{}",
        &md[md.find("Artifacts").unwrap()..md.find("Artifacts").unwrap() + 40]
    );
}

#[test]
fn daily_digest_counts_the_day_with_new_fields() {
    let (home, _pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    let dg = dossier::daily_digest(&conn, "2026-08-23").unwrap();
    assert_eq!(dg["sessions"], 2);
    assert_eq!(dg["messages"], 4);
    assert_eq!(dg["memories_added"], 3);
    assert_eq!(dg["decisions_added"], 3);
    assert_eq!(dg["artifacts_added"], 1);
    assert_eq!(dg["projects"].as_array().unwrap().len(), 1);
    assert_eq!(dg["open_tasks"].as_array().unwrap().len(), 0);
    assert_eq!(dg["recent_handoffs"].as_array().unwrap().len(), 1);
    let activity = dg["project_activity"].as_array().unwrap();
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0]["project"], "dos-proj");
    assert_eq!(activity[0]["sessions"], 2);
    assert_eq!(activity[0]["agents"][0]["agent"], "claude");
    assert_eq!(activity[0]["activities"].as_array().unwrap().len(), 2);
    assert_eq!(activity[0]["activities"][0]["tail"], "卷宗回复二");
    assert_eq!(
        activity[0]["artifacts"][0]["path"],
        "/tmp/dos-proj/report.md"
    );
    assert_eq!(activity[0]["latest_handoff"]["title"], "卷宗功能动工");
    // 别的日子应为零
    let empty = dossier::daily_digest(&conn, "2026-01-01").unwrap();
    assert_eq!(empty["sessions"], 0);
    // markdown 导出（§7.3）
    let md = dossier::render_digest_markdown(&dg);
    assert!(md.contains("# 动态：2026-08-23"));
    assert!(md.contains("决策/规则 3"));
}

#[test]
fn read_session_focus_line_loads_early_lines() {
    // 长会话默认只回尾部；focus_line 保证目标行（哪怕在头部）进入窗口
    let (home, _pid) = fixture();
    // 造一个 20 行的会话
    let body: String = (1..=20)
        .map(|i| format!(
            r#"{{"type":"user","cwd":"/tmp/dos-proj","uuid":"f{i}","timestamp":"2026-08-23T1{:02}:00:00Z","message":{{"role":"user","content":"第 {i} 行内容"}}}}"#,
            i % 10
        ))
        .collect::<Vec<_>>()
        .join("\n");
    let src = tempfile::tempdir().unwrap();
    let dir = src.path().join("claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("long.jsonl"), body + "\n").unwrap();
    let mut c = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, dir)];
    ingest::import_all(&mut c, home.path(), &roots, None, None).unwrap();
    drop(c);

    let conn = db::open(home.path()).unwrap();
    let d = db::read_session(&conn, "claude:long", 5, None, false).unwrap();
    let lines: Vec<i64> = d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line_no"].as_i64().unwrap())
        .collect();
    assert_eq!(lines.first(), Some(&16), "默认尾部窗口: {lines:?}");
    assert!(!lines.contains(&2));
    let d = db::read_session(&conn, "claude:long", 5, Some(2), false).unwrap();
    let lines: Vec<i64> = d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line_no"].as_i64().unwrap())
        .collect();
    assert!(lines.contains(&2), "focus_line=2 必须包含行 2: {lines:?}");
    assert_eq!(*lines.last().unwrap(), 2, "窗口以目标行为止: {lines:?}");
}

#[test]
fn mcp_get_dossier_parity() {
    // §7.2 CLI/MCP 对等
    let (home, _pid) = fixture();
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "get_dossier", "arguments": { "project": "dos-proj" } }
    });
    let resp = yourmem::mcp::handle(home.path(), &req).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let d: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(d["overview"]["sessions"], 2);
    assert_eq!(d["project"], "dos-proj");
}

#[test]
fn memory_status_all_returns_full_chain_across_statuses() {
    // codex 二审点名的测试缺口：status="all" 必须跨状态返回完整演进链
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    let all = db::list_memories(
        &conn,
        &db::MemoryFilter {
            project_id: Some(pid),
            scope: None,
            r#type: None,
            status: Some("all".into()),
            agent: None,
            include_global: false,
            limit: 50,
        },
    )
    .unwrap();
    let statuses: Vec<&str> = all.iter().map(|m| m["status"].as_str().unwrap()).collect();
    assert!(
        statuses.contains(&"superseded"),
        "旧决策必须在: {statuses:?}"
    );
    assert!(statuses.contains(&"confirmed"), "链头必须在: {statuses:?}");
    // 链完整性（codex 三审：断言具体 ID，子集闭合不算数）：v1/v2/v3 三条都在
    let v_ids: Vec<String> = ["存会话（v1", "的 v2", "（v3）"]
        .iter()
        .map(|frag| {
            conn.query_row(
                "SELECT id FROM memories WHERE content LIKE '%' || ?1 || '%'",
                rusqlite::params![frag],
                |r| r.get(0),
            )
            .unwrap()
        })
        .collect();
    for id in &v_ids {
        assert!(
            all.iter().any(|m| m["id"].as_str() == Some(id.as_str())),
            "链成员 {id} 缺失"
        );
    }
    let ids: Vec<String> = all
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    for m in &all {
        if let Some(by) = m["superseded_by"].as_str() {
            assert!(
                ids.contains(&by.to_string()),
                "链断：{} 的取代者 {} 不在结果集",
                m["id"],
                by
            );
        }
    }
    // >200 条截断回归（codex 三审）：大量记忆下 limit=0 必须返回全部、
    // 最旧链尾不丢
    let v1_id: String = conn
        .query_row(
            "SELECT id FROM memories WHERE content LIKE '%v1%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for i in 0..205 {
        conn.execute(
            "INSERT INTO memories(id, scope, type, content, status, created_at, updated_at)
             VALUES (?1, 'global', 'fact', ?2, 'confirmed', ?3, ?3)",
            // 噪声时间统一晚于 v1（更新时间序里排更前，截断先丢的是 v1）
            rusqlite::params![
                format!("mem_noise_{i}"),
                format!("噪声记忆 {i}"),
                format!("2026-08-23T23:{:02}:00Z", i % 60)
            ],
        )
        .unwrap();
    }
    let all2 = db::list_memories(
        &conn,
        &db::MemoryFilter {
            project_id: None,
            scope: None,
            r#type: None,
            status: Some("all".into()),
            agent: None,
            include_global: true,
            limit: 0,
        },
    )
    .unwrap();
    assert!(all2.len() >= 208, "limit=0 不截断: {}", all2.len());
    assert!(
        all2.iter()
            .any(|m| m["id"].as_str() == Some(v1_id.as_str())),
        "最旧链尾 v1 必须在"
    );
}

// S11 回归：时间线的 link_type 与 parent_session_id 必须来自同一条 link——
// 一 child 多链接时两个独立 LIMIT 1 子查询可能各取各行，拼出假父子
#[test]
fn dossier_timeline_link_pair_comes_from_same_row() {
    let (home, pid) = fixture();
    // 第三个会话作为 child，挂两条父链（不同 parent + 不同 link_type）
    let src = tempfile::tempdir().unwrap();
    let dir = src.path().join("claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("dos3.jsonl"),
        "{\"type\":\"user\",\"cwd\":\"/tmp/dos-proj\",\"uuid\":\"d5\",\"timestamp\":\"2026-08-23T11:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"卷宗项目的第三条\"}}\n",
    )
    .unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, dir)];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();

    conn.execute(
        "INSERT INTO session_links(child_session_id, parent_session_id, link_type, created_at)
         VALUES ('claude:dos3', 'claude:dos1', 'fork', '2026-08-23T11:01:00Z'),
                ('claude:dos3', 'claude:dos2', 'continuation', '2026-08-23T11:02:00Z')",
        [],
    )
    .unwrap();

    let d = dossier::project_dossier(&conn, pid).unwrap();
    let entry = d["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == "claude:dos3")
        .expect("dos3 必须在时间线里");
    let pair = (
        entry["link_type"].as_str().unwrap(),
        entry["parent_session_id"].as_str().unwrap(),
    );
    assert!(
        pair == ("fork", "claude:dos1") || pair == ("continuation", "claude:dos2"),
        "link_type 与 parent 必须成对来自同一条 link，实际: {pair:?}"
    );
}

// S15 回归：导出 markdown 对自由文本内联化——换行折叠（防列表断裂/"## " 章节注入），
// 反引号 code span 按最长串加围
#[test]
fn render_markdown_sanitizes_free_text() {
    let (home, pid) = fixture();
    let conn = db::open(home.path()).unwrap();
    db::save_memory(
        &conn,
        &db::MemoryInput {
            project_id: Some(pid),
            scope: "project",
            r#type: "decision",
            content: "第一行\n## 注入标题\n第二行",
            status: None,
            source_session_id: None,
            source_message_id: None,
        },
    )
    .unwrap();
    let id: String = conn
        .query_row(
            "SELECT id FROM memories WHERE content LIKE '%注入标题%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db::update_memory_status(&conn, &id, "confirm", None).unwrap();
    // 含反引号的 artifact 路径
    conn.execute(
        "INSERT INTO session_artifacts(session_id, project_id, path, tool, created_at)
         VALUES ('claude:dos1', ?1, '/tmp/dos-proj/we`ird.md', 'Write', '2026-08-23T09:06:00Z')",
        rusqlite::params![pid],
    )
    .unwrap();

    let d = dossier::project_dossier(&conn, pid).unwrap();
    let md = dossier::render_markdown(&d);
    assert!(
        !md.lines().any(|l| l.starts_with("## 注入标题")),
        "换行必须折叠，不得注入章节标题:\n{md}"
    );
    assert!(
        md.contains("第一行 ## 注入标题 第二行"),
        "内联化后的内容缺失:\n{md}"
    );
    let art_line = md
        .lines()
        .find(|l| l.contains("we`ird"))
        .expect("artifact 行缺失");
    assert!(
        art_line.contains("`` /tmp/dos-proj/we`ird.md ``"),
        "含反引号路径必须加宽围栏: {art_line}"
    );
}

#[test]
fn artifacts_count_on_the_day_they_were_made_not_the_import_day() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dir = src.path().join("claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old.jsonl"), concat!(
        r#"{"type":"user","cwd":"/tmp/art-proj","uuid":"o1","timestamp":"2026-08-23T09:00:00Z","message":{"role":"user","content":"写个报告"}}"#, "\n",
        r#"{"type":"assistant","uuid":"o2","timestamp":"2026-08-23T09:05:00Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/tmp/art-proj/report.md","content":"x"}}]}}"#, "\n",
    )).unwrap();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(
        &mut conn,
        home.path(),
        &[(adapters::AGENT_CLAUDE, dir)],
        None,
        None,
    )
    .unwrap();

    assert_eq!(
        dossier::daily_digest(&conn, "2026-08-23").unwrap()["artifacts_added"],
        1
    );
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert_eq!(
        dossier::daily_digest(&conn, &today).unwrap()["artifacts_added"],
        0,
        "导入当天不算新增"
    );
}

#[test]
fn schema_v13_moves_imported_artifact_times_to_the_session_end() {
    let (home, _pid) = fixture();
    {
        let conn = db::open(home.path()).unwrap();
        conn.execute_batch(
            "UPDATE session_artifacts SET created_at = '2026-10-01T02:00:00.000Z';
             PRAGMA user_version = 12;",
        )
        .unwrap();
    }
    let conn = db::open(home.path()).unwrap();
    let at: String = conn
        .query_row(
            "SELECT created_at FROM session_artifacts WHERE session_id = 'claude:dos1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(at.starts_with("2026-08-23T09:01"), "{at}");
}

fn daily_message_fixture() -> (tempfile::TempDir, rusqlite::Connection, i64) {
    let home = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(db::SCHEMA).unwrap();
    conn.execute_batch("ALTER TABLE projects ADD COLUMN archived_at TEXT")
        .unwrap();
    conn.execute(
        "INSERT INTO projects(name,path,created_at,updated_at)
         VALUES ('daily','/tmp/daily-proj','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
    let pid: i64 = conn
        .query_row("SELECT id FROM projects WHERE name='daily'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let (lo, _) = dossier::day_bounds_utc("2026-08-23").unwrap();
    let lo = yourmem::time::parse_rfc3339(&lo).unwrap();
    let before = (lo - chrono::Duration::hours(1)).to_rfc3339();
    let first = (lo + chrono::Duration::hours(1)).to_rfc3339();
    let offset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
    let second = (lo + chrono::Duration::hours(2))
        .with_timezone(&offset)
        .to_rfc3339();
    let add_session = |id: &str, count: i64| {
        conn.execute(
            "INSERT INTO sessions(id,agent,native_id,project_id,file_path,started_at,ended_at,
                                  message_count,created_at,updated_at)
             VALUES (?1,'claude',?1,?2,?3,?4,?5,?6,?4,?4)",
            rusqlite::params![id, pid, format!("/tmp/{id}.jsonl"), before, first, count],
        )
        .unwrap();
    };
    add_session("claude:cross-day", 4);
    add_session("claude:stale", 1);
    add_session("claude:observed", 1);
    add_session("claude:unknown", 1);
    let add_message = |session: &str, line: i64, timestamp: Option<&str>| {
        conn.execute(
            "INSERT INTO messages(session_id,line_no,kind,content,timestamp)
             VALUES (?1,?2,'user',?3,?4)",
            rusqlite::params![session, line, format!("{session}:{line}"), timestamp],
        )
        .unwrap();
    };
    add_message("claude:cross-day", 1, Some(&before));
    add_message("claude:cross-day", 2, Some(&first));
    add_message("claude:cross-day", 3, Some(&second));
    add_message("claude:cross-day", 4, None);
    add_message("claude:stale", 1, Some(&before));
    add_message("claude:observed", 1, None);
    add_message("claude:unknown", 1, Some("not-rfc3339"));
    conn.execute("UPDATE sessions SET updated_at=?1", [&first])
        .unwrap();
    (home, conn, pid)
}

#[test]
fn daily_digest_uses_message_occurrence_and_separates_time_quality() {
    let (_home, conn, _pid) = daily_message_fixture();
    let digest = dossier::daily_digest(&conn, "2026-08-23").unwrap();
    assert_eq!(digest["sessions"], 1);
    assert_eq!(digest["messages"], 2);
    assert_eq!(digest["observed_only"], 2);
    assert_eq!(digest["observed_only_messages"], 2);
    assert_eq!(digest["unknown_time_messages"], 1);

    let project = &digest["projects"][0];
    assert_eq!(project["sessions"], 1);
    assert_eq!(project["messages"], 2);
    assert_eq!(project["observed_only_messages"], 2);
    assert_eq!(project["unknown_time_messages"], 1);
    let activity = &digest["project_activity"][0];
    assert_eq!(activity["sessions"], project["sessions"]);
    assert_eq!(activity["messages"], project["messages"]);
    assert_eq!(
        activity["observed_only_messages"],
        project["observed_only_messages"]
    );
    assert_eq!(activity["agents"][0]["sessions"], 1);
    assert_eq!(activity["agents"][0]["messages"], 2);
    assert_eq!(activity["agents"][0]["observed_only_messages"], 2);
    assert_eq!(activity["activities"].as_array().unwrap().len(), 1);
    let session = &activity["activities"][0];
    assert_eq!(session["session_id"], "claude:cross-day");
    assert_eq!(session["messages"], 2, "跨日旧消息不得重复计入当天");
    assert_eq!(session["observed_only_messages"], 1);
}

#[test]
fn daily_digest_switches_to_artifact_events_when_table_exists() {
    let (_home, conn, _pid) = daily_message_fixture();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS artifact_events(
             id TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             project_id INTEGER NOT NULL,
             agent TEXT NOT NULL,
             native_tool_call_id TEXT,
             path TEXT NOT NULL,
             from_path TEXT,
             operation TEXT NOT NULL,
             tool TEXT,
             outcome TEXT NOT NULL,
             occurred_at TEXT,
             observed_at TEXT NOT NULL,
             time_quality TEXT NOT NULL,
             source_line_no INTEGER NOT NULL,
             ord INTEGER NOT NULL,
             metadata_json TEXT NOT NULL DEFAULT '{}'
         );",
    )
    .unwrap();
    let (lo, _) = dossier::day_bounds_utc("2026-08-23").unwrap();
    let lo = yourmem::time::parse_rfc3339(&lo).unwrap();
    let inside = (lo + chrono::Duration::hours(3))
        .with_timezone(&chrono::FixedOffset::west_opt(5 * 3600).unwrap())
        .to_rfc3339();
    let outside = (lo - chrono::Duration::seconds(1)).to_rfc3339();
    conn.execute(
        "INSERT INTO artifact_events(id,session_id,project_id,agent,path,tool,occurred_at,observed_at,
                                      time_quality,operation,outcome,source_line_no,ord)
         VALUES ('artifact-new','claude:cross-day',(SELECT id FROM projects WHERE name='daily'),'claude',
                 '/tmp/daily-proj/new.md','Write',?1,?1,'exact','write','success',1,0),
                ('artifact-observed','claude:cross-day',(SELECT id FROM projects WHERE name='daily'),'claude',
                 '/tmp/daily-proj/observed.md','Write',NULL,?1,'observed_only','write','unknown',2,0),
                ('artifact-old','claude:cross-day',(SELECT id FROM projects WHERE name='daily'),'claude',
                 '/tmp/daily-proj/old.md','Write',?2,?2,'exact','write','success',3,0)",
        rusqlite::params![inside, outside],
    ).unwrap();
    conn.execute(
        "INSERT INTO session_artifacts(session_id,project_id,path,tool,created_at)
         VALUES ('claude:cross-day',(SELECT id FROM projects WHERE name='daily'),
                 '/tmp/daily-proj/new.md','Write',?1)",
        rusqlite::params![inside],
    )
    .unwrap();

    let digest = dossier::daily_digest(&conn, "2026-08-23").unwrap();
    assert_eq!(digest["artifacts_added"], 1);
    assert_eq!(digest["observed_only_artifacts"], 1);
    let artifacts = digest["project_activity"][0]["artifacts"]
        .as_array()
        .unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["path"], "/tmp/daily-proj/new.md");
    assert_eq!(artifacts[0]["created_at"], inside);
    assert_eq!(artifacts[0]["time_quality"], "exact");
    assert_eq!(artifacts[0]["operation"], "write");
    assert_eq!(artifacts[0]["outcome"], "success");
}

#[test]
fn digest_excludes_other_days_and_tolerates_unassigned_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let old = db::add_project(&conn, "/tmp/old-project").unwrap().0;
    let current = db::add_project(&conn, "/tmp/current-project").unwrap().0;
    for (sid, pid, time) in [
        ("claude:old", Some(old), "2026-08-01T00:00:00Z"),
        ("claude:current", Some(current), "2026-08-23T00:00:00Z"),
        ("claude:unassigned", None, "2026-08-23T00:00:00Z"),
    ] {
        conn.execute("INSERT INTO sessions(id,agent,native_id,project_id,file_path,started_at,ended_at,message_count,created_at,updated_at)
            VALUES(?1,'claude',?1,?2,'x',?3,?3,1,?3,?3)",rusqlite::params![sid,pid,time]).unwrap();
    }
    conn.execute("INSERT INTO messages(session_id,line_no,kind,content,timestamp) VALUES('claude:old',1,'user','old observed',NULL),('claude:old',2,'user','old invalid','invalid'),('claude:current',1,'user','current','2026-08-23T10:00:00Z'),('claude:unassigned',1,'assistant','write','2026-08-23T10:00:00Z')",[]).unwrap();
    conn.execute("INSERT INTO artifact_events(id,session_id,project_id,agent,path,normalized_path,operation,occurred_at,observed_at,time_quality,source_line_no,ord)
        VALUES('unassigned-artifact','claude:unassigned',NULL,'claude','x','x','write','2026-08-23T10:00:00Z','2026-08-23T10:00:00Z','exact',1,0)",[]).unwrap();
    let digest = dossier::daily_digest(&conn, "2026-08-23").unwrap();
    assert_eq!(digest["messages"], 1);
    assert_eq!(digest["observed_only"], 0);
    assert_eq!(digest["unknown"], 0);
    assert_eq!(digest["projects"].as_array().unwrap().len(), 1);
    assert_eq!(digest["projects"][0]["project"], "current-project");
}

#[test]
fn memory_organize_suggestion_requires_twenty_sessions_and_no_confirmed_project_memory() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let (pid, _) = db::add_project(&conn, "/tmp/research").unwrap();
    let add_session = |i: usize| {
        conn.execute(
            "INSERT INTO sessions(id,agent,native_id,project_id,file_path,started_at,ended_at,message_count,created_at,updated_at)
             VALUES (?1,'codex',?2,?3,?4,'2026-10-24T01:00:00Z','2026-10-24T02:00:00Z',1,'2026-10-24T01:00:00Z','2026-10-24T02:00:00Z')",
            rusqlite::params![format!("codex:s{i}"), format!("s{i}"), pid, format!("/tmp/s{i}.jsonl")],
        ).unwrap();
    };
    for i in 0..19 {
        add_session(i);
    }
    assert_eq!(
        dossier::project_dossier(&conn, pid).unwrap()["memory_organize_suggested"],
        false
    );
    add_session(19);
    let d = dossier::project_dossier(&conn, pid).unwrap();
    assert_eq!(d["memory_organize_suggested"], true);
    assert_eq!(d["confirmed_memories"], 0);

    conn.execute(
        "INSERT INTO memories(id,project_id,scope,type,content,status,created_at,updated_at)
         VALUES ('global',NULL,'global','context','全局记忆','confirmed','2026-10-24T03:00:00Z','2026-10-24T03:00:00Z')",
        [],
    ).unwrap();
    assert_eq!(
        dossier::project_dossier(&conn, pid).unwrap()["memory_organize_suggested"],
        true
    );
    conn.execute(
        "INSERT INTO memories(id,project_id,scope,type,content,status,created_at,updated_at)
         VALUES ('draft',?1,'project','decision','待确认','suggested','2026-10-24T03:00:00Z','2026-10-24T03:00:00Z')",
        [pid],
    ).unwrap();
    assert_eq!(
        dossier::project_dossier(&conn, pid).unwrap()["memory_organize_suggested"],
        true
    );
    conn.execute(
        "UPDATE memories SET status='confirmed' WHERE id='draft'",
        [],
    )
    .unwrap();
    let d = dossier::project_dossier(&conn, pid).unwrap();
    assert_eq!(d["confirmed_memories"], 1);
    assert_eq!(d["memory_organize_suggested"], false);

    conn.execute("UPDATE memories SET status='archived' WHERE id='draft'", [])
        .unwrap();
    db::set_session_deleted(&conn, "codex:s19", true).unwrap();
    assert_eq!(
        dossier::project_dossier(&conn, pid).unwrap()["memory_organize_suggested"],
        false
    );
}
