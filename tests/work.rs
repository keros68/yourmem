use rusqlite::{params, Connection};
use yourmem::{db, models::NewArtifact, work};

fn fixture() -> (tempfile::TempDir, Connection, i64) {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let pid = db::add_project(&conn, home.path().to_string_lossy().as_ref())
        .unwrap()
        .0;
    (home, conn, pid)
}
fn session(conn: &Connection, pid: i64, id: &str, agent: &str) {
    conn.execute("INSERT INTO sessions(id,agent,native_id,project_id,file_path,started_at,ended_at,created_at,updated_at) VALUES(?1,?2,?1,?3,'/tmp/source.jsonl','2026-10-25T01:00:00Z','2026-10-25T02:00:00Z','2026-10-25T01:00:00Z','2026-10-25T02:00:00Z')",params![id,agent,pid]).unwrap();
}
fn message(conn: &Connection, sid: &str, line: i64, kind: &str, content: &str) -> i64 {
    conn.execute("INSERT INTO messages(session_id,line_no,kind,content,timestamp) VALUES(?1,?2,?3,?4,'2026-10-25T01:30:00Z')",params![sid,line,kind,content]).unwrap();
    conn.last_insert_rowid()
}
fn task(conn: &Connection, pid: i64) -> String {
    work::create_task(conn, pid, "实现跨 Agent 任务节点", None, false).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn read_does_not_link_but_explicit_user_marker_does() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "claude:a", "claude");
    work::task_context(&conn, _home.path(), &id, 50).unwrap();
    let linked: i64 = conn
        .query_row("SELECT COUNT(*) FROM task_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(linked, 0);
    message(
        &conn,
        "claude:a",
        1,
        "assistant",
        &format!("yourmem-task:{id}"),
    );
    work::process_session_evidence(&conn, "claude:a").unwrap();
    let linked: i64 = conn
        .query_row("SELECT COUNT(*) FROM task_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(linked, 0, "assistant 文本不能建立关联");
    message(
        &conn,
        "claude:a",
        2,
        "user",
        &format!("继续。\nyourmem-task:{id}"),
    );
    let out = work::process_session_evidence(&conn, "claude:a").unwrap();
    assert_eq!(out["explicit_markers"], 1);
    assert_eq!(
        work::task_context(&conn, _home.path(), &id, 50).unwrap()["sessions"][0]["session_id"],
        "claude:a"
    );
}

#[test]
fn claim_is_hashed_single_use_and_fork_conflict_is_explicit() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "codex:a", "codex");
    session(&conn, pid, "kimi:b", "kimi");
    let claim = work::issue_claim(&conn, &id, "contribution").unwrap();
    let token = claim["claim_token"].as_str().unwrap();
    let stored: String = conn
        .query_row("SELECT token_hash FROM task_claims", [], |r| r.get(0))
        .unwrap();
    assert_ne!(stored, token);
    message(
        &conn,
        "codex:a",
        1,
        "tool_call",
        &format!("get_task_context: {{\"task_id\":\"{id}\",\"claim_token\":\"{token}\"}}"),
    );
    assert_eq!(
        work::process_session_evidence(&conn, "codex:a").unwrap()["claims"],
        1
    );
    assert_eq!(
        work::process_session_evidence(&conn, "codex:a").unwrap()["claims"],
        1,
        "重复采集幂等"
    );
    message(
        &conn,
        "kimi:b",
        1,
        "tool_call",
        &format!("get_task_context: {{\"claim_token\":\"{token}\"}}"),
    );
    let out = work::process_session_evidence(&conn, "kimi:b").unwrap();
    assert_eq!(out["claim_conflicts"], 1);
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT status FROM task_claims", [], |r| r.get(0))
            .unwrap(),
        "claim_conflict"
    );
}

#[test]
fn rejected_relation_is_not_resurrected_by_same_evidence() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "claude:a", "claude");
    work::link_session(&conn, &id, "claude:a", "continuation", "explicit_handoff").unwrap();
    work::unlink_session(&conn, &id, "claude:a").unwrap();
    message(&conn, "claude:a", 1, "user", &format!("yourmem-task:{id}"));
    work::process_session_evidence(&conn, "claude:a").unwrap();
    let state: String = conn
        .query_row("SELECT state FROM task_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "rejected");
}

#[test]
fn user_lock_creates_proposal_and_revision_is_checked() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    work::update_task(&conn, &id, None, Some("completed"), Some(1), None, true).unwrap();
    let proposed = work::update_task(&conn, &id, None, Some("open"), Some(2), None, false).unwrap();
    assert_eq!(proposed["proposal_pending"], true);
    assert_eq!(work::task(&conn, &id).unwrap()["status"], "completed");
    let proposal = conn
        .query_row::<String, _, _>("SELECT id FROM task_state_proposals", [], |r| r.get(0))
        .unwrap();
    work::resolve_proposal(&conn, &proposal, false).unwrap();
    assert!(work::update_task(&conn, &id, None, Some("open"), Some(1), None, false).is_err());
}

#[test]
fn verification_requires_real_success_and_manual_is_user_only() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "codex:a", "codex");
    work::link_session(&conn, &id, "codex:a", "contribution", "user_confirmed").unwrap();
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,native_tool_call_id) VALUES('codex:a',1,0,'tool_call','Bash: cargo test','c1')",[]).unwrap();
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,timestamp) VALUES('codex:a',1,1,'tool_result','{\"call_id\":\"c1\",\"exit_code\":0}','2026-10-25T01:30:00Z')",[]).unwrap();
    let mid=conn.last_insert_rowid();
    assert!(
        work::add_verification(&conn, &id, "manual", "passed", None, "用户确认", false).is_err()
    );
    assert!(work::add_verification(&conn, &id, "test", "passed", None, "", false).is_err());
    let v = work::add_verification(&conn, &id, "test", "passed", Some(mid), "", false).unwrap();
    assert_eq!(v["result"], "passed");
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,native_tool_call_id) VALUES('codex:a',2,0,'tool_call','Bash: ls','c2')",[]).unwrap();
    let arbitrary = message(&conn,"codex:a",3,"tool_result",r#"{"call_id":"c2","exit_code":0}"#);
    assert!(work::add_verification(&conn,&id,"test","passed",Some(arbitrary),"",false).is_err(),"任意成功命令不能充当测试证据");
    work::insert_artifacts(&conn,"codex:a",Some(pid),"codex",&[NewArtifact{
        path:"src/next.rs".into(),from_path:None,tool:"Edit".into(),operation:"edit".into(),
        native_tool_call_id:Some("c3".into()),timestamp:Some("2026-10-25T01:45:00Z".into()),line_no:4,ord:0,
    }]).unwrap();
    assert_eq!(work::task_context(&conn,_home.path(),&id,20).unwrap()["verifications"][0]["verification_state"],"stale_after_modification","过期以证据时间为准，而不是记录创建时间");
    let prose = message(&conn, "codex:a", 5, "tool_result", "all tests passed");
    assert!(work::add_verification(&conn, &id, "test", "passed", Some(prose), "", false).is_err());
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,native_tool_call_id) VALUES('codex:a',6,0,'tool_call','Bash: {\"command\":\"cargo test\"}','real-json')",[]).unwrap();
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,timestamp,native_tool_call_id,tool_success) VALUES('codex:a',6,1,'tool_result','{\"exit_code\":0}','2026-10-25T01:30:00Z','real-json',1)",[]).unwrap();
    let real_mid=conn.last_insert_rowid();
    assert!(work::add_verification(&conn,&id,"test","passed",Some(real_mid),"",false).is_ok());
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,native_tool_call_id) VALUES('codex:a',7,0,'tool_call','[Shell] {\"command\":[\"echo\",\"cargo test\"]}','fake-json')",[]).unwrap();
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,timestamp,native_tool_call_id,tool_success) VALUES('codex:a',7,1,'tool_result','{\"exit_code\":0}','2026-10-25T01:30:00Z','fake-json',1)",[]).unwrap();
    let fake_mid=conn.last_insert_rowid();
    assert!(work::add_verification(&conn,&id,"test","passed",Some(fake_mid),"",false).is_err());
}

#[test]
fn create_task_allows_session_pending_and_timeline_is_newest_first() {
    let (_home,conn,pid)=fixture();
    let created=work::create_task(&conn,pid,"未采集会话任务",Some("claude:not-imported"),false).unwrap();
    assert_eq!(created["session_pending"],true);
    let id=created["task"]["id"].as_str().unwrap();
    session(&conn,pid,"claude:a","claude");
    work::link_session(&conn,id,"claude:a","origin","user_confirmed").unwrap();
    message(&conn,"claude:a",1,"user","较早");
    message(&conn,"claude:a",2,"user","较新");
    let page=work::timeline(&conn,Some(pid),Some(id),None,None,&["message".into()],None,10).unwrap();
    assert_eq!(page["events"][0]["summary"],"较新");
}

#[test]
fn repeated_file_events_keep_time_outcome_and_task_context() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "claude:a", "claude");
    work::link_session(&conn, &id, "claude:a", "contribution", "user_confirmed").unwrap();
    let artifact = |line, call: &str| NewArtifact {
        path: "src/a.rs".into(),
        from_path: None,
        tool: "Edit".into(),
        operation: "edit".into(),
        native_tool_call_id: Some(call.into()),
        timestamp: Some(format!("2026-10-25T01:0{line}:00Z")),
        line_no: line as u64,
        ord: 0,
    };
    work::insert_artifacts(
        &conn,
        "claude:a",
        Some(pid),
        "claude",
        &[artifact(1, "c1"), artifact(2, "c2")],
    )
    .unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artifact_events", [], |r| r.get(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM session_artifacts", [], |r| r.get(0))
            .unwrap(),
        1
    );
    message(
        &conn,
        "claude:a",
        3,
        "tool_result",
        r#"{"tool_use_id":"c2","is_error":true}"#,
    );
    work::infer_artifact_outcomes(&conn, "claude:a").unwrap();
    let context = work::task_context(&conn, _home.path(), &id, 50).unwrap();
    assert_eq!(context["artifacts"].as_array().unwrap().len(), 2);
    assert_eq!(context["artifacts"][1]["outcome"], "failed");
}

#[test]
fn change_log_reports_tombstones_and_expired_cursors() {
    let (_home, conn, pid) = fixture();
    let _id = task(&conn, pid);
    session(&conn, pid, "claude:a", "claude");
    message(&conn, "claude:a", 1, "user", "待删除消息");
    let first = work::changes(&conn, None, 0, 100).unwrap();
    let generation = first["database_generation"].as_str().unwrap().to_string();
    let seq = first["next_cursor"]["seq"].as_i64().unwrap();
    conn.execute("DELETE FROM messages WHERE session_id='claude:a'", [])
        .unwrap();
    let delta = work::changes(&conn, Some(&generation), seq, 100).unwrap();
    assert!(delta["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["operation"] == "delete"));
    work::compact_change_log(&conn, 1).unwrap();
    assert_eq!(
        work::changes(&conn, Some(&generation), 0, 100).unwrap()["reset_required"],
        true
    );
    assert_eq!(
        work::changes(&conn, Some("old-generation"), seq, 100).unwrap()["reset_required"],
        true
    );
}

#[test]
fn duplicate_tasks_need_explicit_override_and_dependencies_reject_cycles() {
    let (_home, conn, pid) = fixture();
    let a = task(&conn, pid);
    let duplicate = work::create_task(&conn, pid, "实现跨 Agent 任务节点", None, false).unwrap();
    assert_eq!(duplicate["created"], false);
    let b = work::create_task(&conn, pid, "实现跨 Agent 任务节点", None, true).unwrap()["task"]
        ["id"]
        .as_str()
        .unwrap()
        .to_string();
    work::add_task_link(&conn, &a, &b, "depends_on", true).unwrap();
    assert!(work::add_task_link(&conn, &b, &a, "depends_on", true).is_err());
}

#[test]
fn timeline_and_records_are_paged_and_pinned_atomically() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "claude:a", "claude");
    work::link_session(&conn, &id, "claude:a", "origin", "user_confirmed").unwrap();
    message(&conn, "claude:a", 1, "user", "开始任务");
    let fields = db::HandoffFields {
        title: "交接",
        next_steps: "继续",
        ..Default::default()
    };
    work::create_handoff(&conn, pid, &fields, Some(&id)).unwrap();
    let page = work::timeline(&conn, Some(pid), Some(&id), None, None, &[], None, 1).unwrap();
    assert_eq!(page["returned_count"], 1);
    assert_eq!(page["has_more"], true);
    let second = work::timeline(
        &conn,
        Some(pid),
        Some(&id),
        None,
        None,
        &[],
        page["next_cursor"].as_str(),
        1,
    )
    .unwrap();
    assert_ne!(
        page["events"][0]["event_key"],
        second["events"][0]["event_key"]
    );
    let records = work::project_records(&conn, pid).unwrap();
    let record = records[0]["record_id"].as_str().unwrap();
    work::pin_record(&conn, pid, "handoff", record).unwrap();
    assert_eq!(
        work::project_records(&conn, pid).unwrap()[0]["is_pinned"],
        true
    );
}

#[test]
fn unclaimed_handoff_appears_in_task_timeline() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    work::create_handoff(&conn, pid, &db::HandoffFields {
        title: "尚未认领的交接", next_steps: "继续", ..Default::default()
    }, Some(&id)).unwrap();
    let page = work::timeline(&conn, Some(pid), Some(&id), None, None, &[], None, 20).unwrap();
    assert!(page["events"].as_array().unwrap().iter().any(|event|
        event["kind"] == "handoff" && event["summary"] == "尚未认领的交接"
    ));
}

#[test]
fn codex_plan_is_session_step_not_project_task() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "codex:a", "codex");
    work::link_session(&conn, &id, "codex:a", "origin", "user_confirmed").unwrap();
    message(
        &conn,
        "codex:a",
        1,
        "tool_call",
        r#"update_plan: {"plan":[{"step":"实现接口","status":"in_progress"},{"step":"运行测试","status":"pending"}]}"#,
    );
    assert_eq!(work::record_session_steps(&conn, "codex:a").unwrap(), 2);
    assert_eq!(work::record_session_steps(&conn, "codex:a").unwrap(), 2);
    let steps = work::steps_for_session(&conn, "codex:a").unwrap();
    assert_eq!(steps.len(), 2, "重复采集不能双录");
    assert_eq!(steps[0]["task_id"], id);
    assert_eq!(
        work::list_tasks(&conn, Some(pid), Some("all"), None, 50)
            .unwrap()
            .len(),
        1,
        "Todo 不进入项目任务板"
    );
}

#[test]
fn native_tool_result_updates_real_artifact_and_stales_verification() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "codex:a", "codex");
    work::link_session(&conn, &id, "codex:a", "origin", "user_confirmed").unwrap();
    conn.execute(
        "UPDATE sessions SET cwd=?2 WHERE id=?1",
        params!["codex:a", _home.path().to_string_lossy()],
    )
    .unwrap();
    let artifact = NewArtifact {
        path: "src/../result.txt".into(),
        from_path: None,
        tool: "apply_patch".into(),
        operation: "edit".into(),
        native_tool_call_id: Some("call-1".into()),
        timestamp: Some("2026-10-25T01:00:00Z".into()),
        line_no: 1,
        ord: 0,
    };
    work::insert_artifacts(&conn, "codex:a", Some(pid), "codex", &[artifact.clone()]).unwrap();
    message(&conn, "codex:a", 2, "tool_call", "Bash: cargo test");
    conn.execute("UPDATE messages SET native_tool_call_id='call-1' WHERE session_id='codex:a' AND line_no=2",[]).unwrap();
    let mid = message(&conn, "codex:a", 3, "tool_result", "exit 0");
    conn.execute(
        "UPDATE messages SET native_tool_call_id='call-1',tool_success=1 WHERE id=?1",
        [mid],
    )
    .unwrap();
    work::infer_artifact_outcomes(&conn, "codex:a").unwrap();
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT outcome FROM artifact_events", [], |r| r.get(0))
            .unwrap(),
        "success"
    );
    assert!(conn
        .query_row::<String, _, _>("SELECT normalized_path FROM artifact_events", [], |r| r
            .get(0))
        .unwrap()
        .ends_with("result.txt"));
    work::add_verification(&conn, &id, "test", "passed", Some(mid), "", false).unwrap();
    let later = NewArtifact {
        timestamp: Some("2100-01-01T00:00:00Z".into()),
        line_no: 3,
        native_tool_call_id: Some("call-2".into()),
        ..artifact
    };
    work::insert_artifacts(&conn, "codex:a", Some(pid), "codex", &[later]).unwrap();
    let context = work::task_context(&conn, _home.path(), &id, 50).unwrap();
    assert_eq!(
        context["verifications"][0]["verification_state"],
        "stale_after_modification"
    );
    conn.execute("UPDATE artifact_events SET occurred_at='2100-01-01T08:00:00+08:00' WHERE native_tool_call_id='call-2'",[]).unwrap();
    assert_eq!(
        work::task_context(&conn, _home.path(), &id, 50).unwrap()["verifications"][0]
            ["verification_state"],
        "stale_after_modification",
        "带时区偏移的后续修改也必须使验证过期"
    );
}

#[test]
fn keyset_page_does_not_repeat_when_earlier_event_arrives() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "claude:a", "claude");
    work::link_session(&conn, &id, "claude:a", "origin", "user_confirmed").unwrap();
    message(&conn, "claude:a", 1, "user", "第一条");
    message(&conn, "claude:a", 2, "user", "第二条");
    let first = work::timeline(
        &conn,
        Some(pid),
        Some(&id),
        None,
        None,
        &["message".into()],
        None,
        1,
    )
    .unwrap();
    conn.execute("INSERT INTO messages(session_id,line_no,kind,content,timestamp) VALUES('claude:a',0,'user','晚到的更早事件','2026-10-25T01:00:00Z')",[]).unwrap();
    let second = work::timeline(
        &conn,
        Some(pid),
        Some(&id),
        None,
        None,
        &["message".into()],
        first["next_cursor"].as_str(),
        10,
    )
    .unwrap();
    let events = second["events"].as_array().unwrap();
    assert!(!events
        .iter()
        .any(|e| e["event_key"] == first["events"][0]["event_key"]));
    assert_eq!(events.len(), 2);
}

#[test]
fn document_collection_failure_is_visible_and_recovers() {
    let (home, conn, pid) = fixture();
    let path = std::path::Path::new(
        &conn
            .query_row::<String, _, _>("SELECT path FROM projects WHERE id=?1", [pid], |r| r.get(0))
            .unwrap(),
    )
    .join("PROGRESS.md");
    std::fs::write(
        &path,
        vec![b'x'; (yourmem::project_docs::MAX_DOC_BYTES + 1) as usize],
    )
    .unwrap();
    yourmem::memfiles::collect(
        &conn,
        home.path(),
        &yourmem::memfiles::SourceDirs::default(),
    )
    .unwrap();
    let docs = yourmem::project_docs::status(&conn, home.path(), pid).unwrap();
    assert_eq!(
        docs["docs"][0]["collection_error"],
        "文件超过 1 MiB，最新内容尚未备份"
    );
    std::fs::write(&path, "最新进度").unwrap();
    yourmem::memfiles::collect(
        &conn,
        home.path(),
        &yourmem::memfiles::SourceDirs::default(),
    )
    .unwrap();
    let docs = yourmem::project_docs::status(&conn, home.path(), pid).unwrap();
    assert!(docs["docs"][0]["collection_error"].is_null());
    assert_eq!(docs["docs"][0]["revisions"], 1);
}

#[test]
fn repeated_open_does_not_rewrite_schema_or_legacy_marker() {
    let (home, conn, _pid) = fixture();
    let schema: i64 = conn.pragma_query_value(None, "schema_version", |row| row.get(0)).unwrap();
    let migrated: String = conn.query_row("SELECT value FROM work_meta WHERE key='legacy_migrated'",[],|row|row.get(0)).unwrap();
    drop(conn);
    let conn = db::open(home.path()).unwrap();
    assert_eq!(conn.pragma_query_value::<i64,_>(None,"schema_version",|row|row.get(0)).unwrap(),schema);
    assert_eq!(conn.query_row::<String,_,_>("SELECT value FROM work_meta WHERE key='legacy_migrated'",[],|row|row.get(0)).unwrap(),migrated);
}

#[test]
fn session_delete_removes_new_evidence_but_keeps_task_history() {
    let (_home,conn,pid)=fixture();
    let id=task(&conn,pid);
    session(&conn,pid,"claude:a","claude");
    work::link_session(&conn,&id,"claude:a","origin","user_confirmed").unwrap();
    work::insert_artifacts(&conn,"claude:a",Some(pid),"claude",&[NewArtifact{
        path:"a.rs".into(),from_path:None,tool:"Write".into(),operation:"create".into(),
        native_tool_call_id:Some("a1".into()),timestamp:None,line_no:1,ord:0,
    }]).unwrap();
    work::record_step_snapshot(&conn,"claude:a",1,None,&[(Some("s1".into()),"step".into(),"pending".into())],Some(&id)).unwrap();
    conn.execute("INSERT INTO task_verifications(id,task_id,kind,result,evidence,session_id,task_revision,created_at) VALUES('v1',?1,'external','unknown','x','claude:a',1,'2026-01-01')",[&id]).unwrap();
    db::delete_session_data(&conn,"claude:a").unwrap();
    for table in ["artifact_occurrences","artifact_events","task_steps","task_sessions","task_verifications"] {
        assert_eq!(conn.query_row::<i64,_,_>(&format!("SELECT COUNT(*) FROM {table}"),[],|row|row.get(0)).unwrap(),0,"{table}");
    }
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM task_events WHERE task_id=?1 AND session_id IS NULL",[&id],|row|row.get(0)).unwrap(),1);
}

#[test]
fn close_work_is_atomic_on_invalid_status() {
    let (_home,conn,pid)=fixture();
    let id=task(&conn,pid);
    let result=work::close_work(&conn,pid,&id,&db::HandoffFields{title:"不应保存",..Default::default()},Some("not-a-status"));
    assert!(result.is_err());
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM handoffs",[],|row|row.get(0)).unwrap(),0);
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM task_claims",[],|row|row.get(0)).unwrap(),0);
}

#[test]
fn close_work_allows_unchanged_status_and_unimported_session() {
    let (_home,conn,pid)=fixture();
    let id=task(&conn,pid);
    let result=work::close_work(&conn,pid,&id,&db::HandoffFields{title:"保存交接",session_id:Some("claude:not-imported"),..Default::default()},Some("open")).unwrap();
    assert_eq!(result["handoff"]["session_pending"],true);
    assert!(result["task_update"].is_null());
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM handoffs",[],|row|row.get(0)).unwrap(),1);
}

#[test]
fn stateful_steps_replay_across_import_batches() {
    let (_home,conn,pid)=fixture();
    session(&conn,pid,"claude:a","claude");
    message(&conn,"claude:a",1,"tool_call",r#"TaskCreate: {"activeForm":"实现","description":"实现","subject":"实现"}"#);
    work::record_session_steps(&conn,"claude:a").unwrap();
    work::mark_session_evidence_processed(&conn,"claude:a").unwrap();
    message(&conn,"claude:a",2,"tool_call",r#"TaskUpdate: {"status":"completed","taskId":"1"}"#);
    work::record_session_steps(&conn,"claude:a").unwrap();
    let steps=work::steps_for_session(&conn,"claude:a").unwrap();
    assert_eq!(steps.len(),1);
    assert_eq!(steps[0]["status"],"completed");
}

#[test]
fn claim_expiry_uses_native_call_time() {
    let (_home,conn,pid)=fixture();
    let id=task(&conn,pid);
    session(&conn,pid,"claude:a","claude");
    let claim=work::issue_claim(&conn,&id,"contribution").unwrap();
    conn.execute("UPDATE task_claims SET expires_at='2020-01-01T00:00:00Z'",[]).unwrap();
    let result=work::claim_from_tool_call(&conn,"claude:a",Some("2019-12-31T00:00:00Z"),claim["claim_token"].as_str().unwrap(),Some("native-1")).unwrap();
    assert_eq!(result["claimed"],true);
}

#[test]
fn native_artifact_ids_are_scoped_and_only_exact_forks_dedupe() {
    let (_home,conn,pid)=fixture();
    session(&conn,pid,"claude:a","claude");
    session(&conn,pid,"claude:b","claude");
    let artifact=|time:&str|NewArtifact{path:"a.rs".into(),from_path:None,tool:"Write".into(),operation:"create".into(),native_tool_call_id:Some("same".into()),timestamp:Some(time.into()),line_no:1,ord:0};
    work::insert_artifacts(&conn,"claude:a",Some(pid),"claude",&[artifact("2026-01-01T00:00:00Z")]).unwrap();
    work::insert_artifacts(&conn,"claude:b",Some(pid),"claude",&[artifact("2026-01-01T00:00:00Z"),artifact("2026-01-02T00:00:00Z")]).unwrap();
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM artifact_events",[],|row|row.get(0)).unwrap(),2);
    message(&conn,"claude:b",2,"tool_result",r#"{"call_id":"same","exit_code":0}"#);
    work::infer_artifact_outcomes(&conn,"claude:b").unwrap();
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM artifact_events WHERE session_id='claude:b' AND outcome='success'",[],|row|row.get(0)).unwrap(),1);
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM artifact_events WHERE session_id='claude:a' AND outcome='unknown'",[],|row|row.get(0)).unwrap(),1);
}

#[test]
fn legacy_task_identity_is_stable_across_databases() {
    let (home_a,conn_a,pid_a)=fixture();
    let (home_b,conn_b,pid_b)=fixture();
    for (conn,pid) in [(&conn_a,pid_a),(&conn_b,pid_b)] {
        conn.execute("INSERT INTO memories(id,project_id,scope,type,content,status,created_at,updated_at) VALUES('mem_legacy',?1,'project','task','legacy','confirmed','2026-01-01','2026-01-01')",[pid]).unwrap();
        work::migrate_legacy_tasks(conn).unwrap();
    }
    let a=work::list_tasks(&conn_a,Some(pid_a),Some("all"),None,10).unwrap();
    let b=work::list_tasks(&conn_b,Some(pid_b),Some("all"),None,10).unwrap();
    assert_eq!(a[0]["id"],b[0]["id"]);
    let event_a:String=conn_a.query_row("SELECT id FROM task_events WHERE task_id=?1",[a[0]["id"].as_str().unwrap()],|row|row.get(0)).unwrap();
    let event_b:String=conn_b.query_row("SELECT id FROM task_events WHERE task_id=?1",[b[0]["id"].as_str().unwrap()],|row|row.get(0)).unwrap();
    assert_eq!(event_a,event_b);
    drop((home_a,home_b));
}

#[test]
fn locked_task_keeps_title_edit_and_title_only_update_keeps_completion_time() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    work::update_task(&conn, &id, None, Some("completed"), None, None, true).unwrap();
    let done_at = work::task(&conn, &id).unwrap()["completed_at"].clone();
    assert!(done_at.is_string());
    let r = work::update_task(&conn, &id, Some("新标题"), Some("in_progress"), None, None, false).unwrap();
    assert_eq!(r["proposal_pending"], true);
    let t = work::task(&conn, &id).unwrap();
    assert_eq!(t["title"], "新标题", "锁定任务上标题修改不得被静默丢弃");
    assert_eq!(t["status"], "completed");
    assert_eq!(t["completed_at"], done_at, "只改标题不得改写完成时间");
}

#[test]
fn foreign_project_marker_and_claim_do_not_fail_evidence_processing() {
    let (_home, conn, pid) = fixture();
    let other = db::add_project(&conn, "/tmp/yourmem-other-project").unwrap().0;
    let foreign = task(&conn, other);
    let claim = work::issue_claim(&conn, &foreign, "continuation").unwrap();
    let token = claim["claim_token"].as_str().unwrap().to_string();
    session(&conn, pid, "claude:a", "claude");
    message(&conn, "claude:a", 1, "user", &format!("继续 yourmem-task:{foreign}"));
    conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,native_tool_call_id) VALUES('claude:a',2,0,'tool_call',?1,'c1')",
        [format!("mcp__yourmem__get_task_context: {{\"claim_token\":\"{token}\"}}")]).unwrap();
    let r = work::process_session_evidence(&conn, "claude:a").unwrap();
    assert_eq!(r["claims"], 0);
    assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM task_sessions WHERE session_id='claude:a'",[],|r|r.get(0)).unwrap(), 0);
    assert_eq!(conn.query_row::<String,_,_>("SELECT status FROM task_claims",[],|r|r.get(0)).unwrap(), "pending");
}

#[test]
fn test_command_detection_handles_wrappers_and_word_boundaries() {
    let (_home, conn, pid) = fixture();
    let id = task(&conn, pid);
    session(&conn, pid, "codex:a", "codex");
    work::link_session(&conn, &id, "codex:a", "contribution", "user_confirmed").unwrap();
    let mut line = 0;
    let mut verify = |call: &str| {
        line += 2;
        let cid = format!("call{line}");
        conn.execute("INSERT INTO messages(session_id,line_no,ord,kind,content,native_tool_call_id) VALUES('codex:a',?1,0,'tool_call',?2,?3)",params![line,call,cid]).unwrap();
        let mid = message(&conn, "codex:a", line + 1, "tool_result", &format!("{{\"call_id\":\"{cid}\",\"exit_code\":0}}"));
        work::add_verification(&conn, &id, "test", "passed", Some(mid), "", false).is_ok()
    };
    assert!(verify(r#"shell: {"command":["bash","-lc","cargo test"]}"#));
    assert!(verify(r#"exec_command: {"cmd":"bash -lc 'npm test'"}"#));
    assert!(verify(r#"[Shell] {"command":"cd x && pytest -q"}"#));
    assert!(!verify(r#"Bash: {"command":"cargo testing-tool"}"#));
}
