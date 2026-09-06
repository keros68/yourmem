//! 任务状态时间线（DESIGN-0.3 §7.7，0.4.3，LoopX 触发立项）：从 `messages.tool_call`
//! 读时派生 agent 自报的任务状态——零采集改动、零 schema 变更。
//!
//! 形态按真机样本固化（2026-08-31 本库解剖，41/329 活跃会话）：
//! - 全量覆盖快照：zcode `[TodoWrite]`、opencode `todowrite:`、claude legacy
//!   `TodoWrite:`（content 字段）、kimi `[TodoList]`（**title** 字段，状态用 **done**）
//! - 按 id 打补丁：hermes `[todo]` 的 `merge:true` 变体只携带变化项；空对象 `{}` 忽略
//! - 隐式编号增量：claude `TaskCreate`（无 id 字段，按创建顺序编号）/`TaskUpdate`
//!   （按 taskId 改状态/主题）
//! - codex `update_plan` 按样本门禁留空（本库 0/76，原始 rollout 零命中）——刻意
//!   不匹配，防正文提及 "plan" 误报
//!
//! 解析防御式：坏 JSON / 未知形状 / 缺字段一律跳过，绝不 panic（参照 zcode 格式
//! 漂移教训：形态走样时宁可漏一条，不可错一条）。fixture 全部来自真机样本脱敏。

use serde_json::Value;

/// 归一化任务项。status 统一为 pending | in_progress | completed（kimi 的 done
/// 折算成 completed，其余原样透传——未知状态宁可显示出来，不可猜）。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TaskItem {
    pub id: Option<String>,
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TaskEvent {
    /// 全量覆盖快照（zcode/kimi/opencode/claude TodoWrite/hermes merge 非 true）
    Snapshot(Vec<TaskItem>),
    /// hermes merge:true：按 id upsert 的部分补丁
    Patch(Vec<TaskItem>),
    /// claude TaskCreate：隐式编号追加
    Create { content: String },
    /// claude TaskUpdate：按 taskId 改状态/主题
    Update {
        task_id: String,
        status: Option<String>,
        subject: Option<String>,
    },
}

/// 一个变化点（相邻同状态快照去重后）
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChangePoint {
    pub line_no: i64,
    pub timestamp: Option<String>,
    pub todos: Vec<TaskItem>,
}

/// project_dossier 的 SQL 过滤片段：agent 门控的前缀匹配，防跨源误配
/// （kimi 也用方括号格式，若不按 agent 圈定，`[todo]` 会撞上未来其他源的同类工具）。
pub(super) const MATCHER_SQL: &str = "\
    (s.agent='zcode' AND m.content LIKE '[TodoWrite] {%') \
    OR (s.agent='kimi' AND m.content LIKE '[TodoList] {%') \
    OR (s.agent='hermes' AND m.content LIKE '[todo] {%') \
    OR (s.agent='opencode' AND m.content LIKE 'todowrite: {%') \
    OR (s.agent='claude' AND (m.content LIKE 'TaskCreate: {%' \
        OR m.content LIKE 'TaskUpdate: {%' OR m.content LIKE 'TodoWrite: {%'))";

/// 单条 tool_call → 任务事件。agent/前缀不认识、JSON 坏、形状不对 → None。
pub(super) fn parse_event(agent: &str, content: &str) -> Option<TaskEvent> {
    let body = json_body(content)?;
    match agent {
        "zcode" => snapshot_event(&body, "content"),
        "kimi" => snapshot_event(&body, "title"),
        "opencode" => snapshot_event(&body, "content"),
        "hermes" => {
            if body["merge"].as_bool() == Some(true) {
                Some(TaskEvent::Patch(extract_todos(&body, "content")?))
            } else {
                snapshot_event(&body, "content")
            }
        }
        "claude" => {
            if content.starts_with("TaskCreate: {") {
                // 真机样本字段序：subject / activeForm / description，subject 是任务名
                let name = body["subject"]
                    .as_str()
                    .or_else(|| body["activeForm"].as_str())
                    .or_else(|| body["description"].as_str())?;
                Some(TaskEvent::Create {
                    content: name.to_string(),
                })
            } else if content.starts_with("TaskUpdate: {") {
                Some(TaskEvent::Update {
                    task_id: body["taskId"].as_str()?.to_string(),
                    status: body["status"].as_str().map(normalize_status),
                    subject: body["subject"].as_str().map(str::to_string),
                })
            } else if content.starts_with("TodoWrite: {") {
                // legacy 形态：本库零样本，schema 与 opencode/zcode（同为 claude 系拷贝）一致
                snapshot_event(&body, "content")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 折叠一个会话的事件流（调用方保证已按 line_no, ord 排序）为变化点序列：
/// 全量快照直接替换状态；hermes 补丁按 id upsert；claude 增量按创建顺序编号重放；
/// 相邻同状态去重——末个变化点即最终任务状态。
pub(super) fn fold(events: Vec<(i64, Option<String>, TaskEvent)>) -> Vec<ChangePoint> {
    let mut state: Vec<TaskItem> = Vec::new();
    let mut seq: u64 = 0;
    let mut out: Vec<ChangePoint> = Vec::new();
    for (line_no, timestamp, ev) in events {
        match ev {
            TaskEvent::Snapshot(items) => {
                seq = items.len() as u64;
                state = items;
            }
            TaskEvent::Patch(items) => {
                for it in items {
                    let Some(id) = it.id.clone() else { continue }; // 无 id 无法锚定：丢弃
                    if let Some(slot) = state.iter_mut().find(|s| s.id.as_deref() == Some(id.as_str())) {
                        slot.content = it.content;
                        slot.status = it.status;
                    } else {
                        state.push(it);
                    }
                }
            }
            TaskEvent::Create { content } => {
                seq += 1;
                state.push(TaskItem {
                    id: Some(seq.to_string()),
                    content,
                    status: "pending".into(),
                });
            }
            TaskEvent::Update {
                task_id,
                status,
                subject,
            } => {
                let Some(slot) = state
                    .iter_mut()
                    .find(|s| s.id.as_deref() == Some(task_id.as_str()))
                else {
                    continue; // 未知 taskId（如跨会话引用）：防御性忽略
                };
                if let Some(st) = status {
                    slot.status = st;
                }
                if let Some(sub) = subject {
                    slot.content = sub;
                }
            }
        }
        if out.last().map(|c| &c.todos) == Some(&state) {
            continue; // 相邻同状态：不入变化点
        }
        out.push(ChangePoint {
            line_no,
            timestamp,
            todos: state.clone(),
        });
    }
    out
}

fn json_body(content: &str) -> Option<Value> {
    let pos = content.find('{')?;
    serde_json::from_str(&content[pos..]).ok()
}

fn snapshot_event(body: &Value, field: &str) -> Option<TaskEvent> {
    Some(TaskEvent::Snapshot(extract_todos(body, field)?))
}

/// 缺 todos 键（如 hermes 的 `{}`）→ None；空数组是合法快照（清空任务列表）。
fn extract_todos(body: &Value, field: &str) -> Option<Vec<TaskItem>> {
    let arr = body["todos"].as_array()?;
    let mut items = Vec::new();
    for t in arr {
        let Some(text) = t[field].as_str() else { continue };
        items.push(TaskItem {
            id: t["id"].as_str().map(str::to_string),
            content: text.to_string(),
            status: t["status"].as_str().map(normalize_status).unwrap_or_else(|| "pending".into()),
        });
    }
    Some(items)
}

fn normalize_status(s: &str) -> String {
    match s {
        "done" => "completed".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— 以下 fixture 均来自 2026-08-31 真机样本，任务文本已脱敏 ——

    #[test]
    fn zcode_full_snapshot_with_priority() {
        let ev = parse_event(
            "zcode",
            r#"[TodoWrite] {"todos":[{"content":"任务A","priority":"high","status":"completed"},{"content":"任务B","priority":"high","status":"in_progress"}]}"#,
        ).unwrap();
        let TaskEvent::Snapshot(items) = ev else { panic!("应为快照") };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "任务A");
        assert_eq!(items[0].status, "completed");
        assert_eq!(items[1].status, "in_progress");
        assert!(items[0].id.is_none());
    }

    #[test]
    fn kimi_uses_title_field_and_done_status() {
        let ev = parse_event(
            "kimi",
            r#"[TodoList] {"todos":[{"status":"done","title":"任务A"},{"status":"pending","title":"任务B"}]}"#,
        ).unwrap();
        let TaskEvent::Snapshot(items) = ev else { panic!("应为快照") };
        assert_eq!(items[0].content, "任务A");
        assert_eq!(items[0].status, "completed", "kimi 的 done 应归一化为 completed");
        assert_eq!(items[1].status, "pending");
    }

    #[test]
    fn opencode_lowercase_prefix_content_field() {
        let ev = parse_event(
            "opencode",
            r#"todowrite: {"todos":[{"content":"Create index.html","priority":"high","status":"pending"}]}"#,
        ).unwrap();
        let TaskEvent::Snapshot(items) = ev else { panic!("应为快照") };
        assert_eq!(items[0].content, "Create index.html");
    }

    #[test]
    fn hermes_merge_patch_upserts_by_id() {
        // 真机形态：全量 6 项 → merge:true 只带 2 项变化
        let full = parse_event(
            "hermes",
            r#"[todo] {"todos": [{"id": "1", "content": "任务A", "status": "in_progress"}, {"id": "2", "content": "任务B", "status": "pending"}], "merge": false}"#,
        ).unwrap();
        let patch = parse_event(
            "hermes",
            r#"[todo] {"merge": true, "todos": [{"content": "任务A", "id": "1", "status": "completed"}]}"#,
        ).unwrap();
        let changes = fold(vec![
            (10, None, full),
            (20, None, patch),
        ]);
        assert_eq!(changes.len(), 2);
        let final_items = &changes.last().unwrap().todos;
        assert_eq!(final_items.len(), 2, "补丁不应清掉未提及的任务");
        assert_eq!(final_items[0].status, "completed");
        assert_eq!(final_items[1].status, "pending");
    }

    #[test]
    fn hermes_empty_object_is_ignored() {
        assert_eq!(parse_event("hermes", "[todo] {}"), None);
    }

    #[test]
    fn claude_incremental_replay_assigns_sequential_ids() {
        let mk = |n: &str| {
            parse_event(
                "claude",
                &format!(r#"TaskCreate: {{"activeForm":"表单{n}","description":"描述{n}","subject":"任务{n}"}}"#),
            )
            .unwrap()
        };
        let changes = fold(vec![
            (1, None, mk("甲")),
            (2, None, mk("乙")),
            (3, None, parse_event("claude", r#"TaskUpdate: {"status":"in_progress","taskId":"1"}"#).unwrap()),
            (4, None, parse_event("claude", r#"TaskUpdate: {"status":"completed","taskId":"1"}"#).unwrap()),
        ]);
        // 创建×2 + 状态改×2，每次都改变状态 → 4 个变化点
        assert_eq!(changes.len(), 4);
        let final_items = &changes.last().unwrap().todos;
        assert_eq!(final_items.len(), 2);
        assert_eq!(final_items[0].id.as_deref(), Some("1"), "隐式编号按创建顺序");
        assert_eq!(final_items[0].status, "completed");
        assert_eq!(final_items[1].status, "pending");
    }

    #[test]
    fn claude_update_unknown_task_id_is_ignored() {
        let changes = fold(vec![(
            1,
            None,
            parse_event("claude", r#"TaskUpdate: {"status":"completed","taskId":"9"}"#).unwrap(),
        )]);
        assert!(changes.is_empty(), "未知 taskId 不应产生任何状态");
    }

    #[test]
    fn adjacent_identical_snapshots_dedupe() {
        let snap = |st: &str| {
            parse_event("kimi", &format!(r#"[TodoList] {{"todos":[{{"status":"{st}","title":"任务A"}}]}}"#)).unwrap()
        };
        let changes = fold(vec![
            (1, None, snap("in_progress")),
            (2, None, snap("in_progress")), // 全量覆盖写导致的重复
            (3, None, snap("done")),
        ]);
        assert_eq!(changes.len(), 2, "相邻同状态只留一个变化点");
        assert_eq!(changes.last().unwrap().todos[0].status, "completed");
    }

    #[test]
    fn codex_and_other_tools_never_match() {
        // codex 按样本门禁刻意不匹配（正文含 update_plan 也不行）
        assert_eq!(
            parse_event("codex", r#"update_plan: {"plan":[{"step":"任务A","status":"completed"}]}"#),
            None
        );
        // claude 的普通工具不误配
        assert_eq!(parse_event("claude", r#"Bash: {"command":"ls TaskCreate: {}"}"#), None);
        assert_eq!(parse_event("hermes", r#"[terminal] {"command":"todo"}"#), None);
    }

    #[test]
    fn malformed_json_and_unknown_shapes_are_defensive() {
        assert_eq!(parse_event("kimi", "[TodoList] {\"todos\":"), None, "坏 JSON");
        assert_eq!(parse_event("kimi", "[TodoList] null"), None);
        assert_eq!(parse_event("kimi", "[TodoList] {\"other\":1}"), None, "缺 todos 键");
        assert_eq!(parse_event("codex", "[TodoWrite] {\"todos\":[]}"), None, "agent 门控");
    }

    #[test]
    fn integration_through_project_dossier() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{}{}", crate::db::SCHEMA, crate::db::TRIGGERS_SQL)).unwrap();
        conn.execute(
            "INSERT INTO projects(path, name, created_at, updated_at) VALUES ('/tmp/p', 'p', '2026-08-31T00:00:00Z', '2026-08-31T00:00:00Z')",
            [],
        )
        .unwrap();
        let session = |id: &str, agent: &str| {
            conn.execute(
                "INSERT INTO sessions(id, agent, native_id, project_id, file_path, created_at, updated_at, started_at)
                 VALUES (?1, ?2, ?3, 1, '/tmp/f', '2026-08-31T00:00:00Z', '2026-08-31T00:00:00Z', '2026-08-30T10:00:00Z')",
                rusqlite::params![id, agent, id],
            )
            .unwrap();
        };
        let msg = |sid: &str, line: i64, content: &str| {
            conn.execute(
                "INSERT INTO messages(session_id, line_no, ord, kind, content) VALUES (?1, ?2, 0, 'tool_call', ?3)",
                rusqlite::params![sid, line, content],
            )
            .unwrap();
        };
        session("kimi:s1", "kimi");
        msg("kimi:s1", 1, r#"[TodoList] {"todos":[{"status":"in_progress","title":"任务A"},{"status":"pending","title":"任务B"}]}"#);
        msg("kimi:s1", 2, r#"[TodoList] {"todos":[{"status":"in_progress","title":"任务A"},{"status":"pending","title":"任务B"}]}"#);
        msg("kimi:s1", 3, r#"[TodoList] {"todos":[{"status":"done","title":"任务A"},{"status":"in_progress","title":"任务B"}]}"#);
        session("claude:s2", "claude");
        msg("claude:s2", 1, r#"TaskCreate: {"activeForm":"表单","description":"d","subject":"任务甲"}"#);
        msg("claude:s2", 2, r#"TaskUpdate: {"status":"completed","taskId":"1"}"#);
        session("codex:s3", "codex");
        msg("codex:s3", 1, r#"update_plan: {"plan":[{"step":"任务X","status":"completed"}]}"#);

        let d = super::super::project_dossier(&conn, 1).unwrap();
        let tl = d["task_timeline"].as_array().unwrap();
        assert_eq!(tl.len(), 2, "codex 不得出现，两条带任务调用的会话各一条");
        let kimi = tl.iter().find(|t| t["agent"] == "kimi").unwrap();
        assert_eq!(kimi["changes"].as_array().unwrap().len(), 2, "相邻重复快照折叠");
        assert_eq!(kimi["final"]["todos"][0]["status"], "completed");
        assert_eq!(kimi["done"], 1);
        assert_eq!(kimi["total"], 2);
        assert!(kimi["final"]["line_no"].as_i64().unwrap() > 0, "可溯源：带行号");
        let claude = tl.iter().find(|t| t["agent"] == "claude").unwrap();
        assert_eq!(claude["final"]["todos"][0]["status"], "completed");
        assert_eq!(claude["final"]["todos"][0]["id"], "1");

        let md = super::super::render_markdown(&d);
        assert!(md.contains("## 任务状态"), "markdown 有任务状态章节\n{md}");
        assert!(md.contains("- [x] 任务A"), "checkbox 列表\n{md}");
        assert!(md.contains("任务B（进行中）"), "in_progress 标注\n{md}");
        assert!(!md.contains("任务X"), "codex 不误报\n{md}");
    }
}
