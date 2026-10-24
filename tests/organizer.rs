use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

use serde_json::json;
use yourmem::{db, dossier, organizer};

fn settings(base_url: String) -> organizer::AiSettings {
    organizer::AiSettings {
        base_url,
        model: "test-model".into(),
        max_input_chars: 40_000,
    }
}

#[test]
fn ai_settings_persist_without_a_secret() {
    let home = tempfile::tempdir().unwrap();
    organizer::save_settings(home.path(), &settings("https://api.example.com/v1".into())).unwrap();
    let raw = std::fs::read_to_string(home.path().join("config.json")).unwrap();
    assert!(raw.contains("api.example.com"));
    assert!(!raw.to_lowercase().contains("api_key"));
    assert_eq!(organizer::load_settings(home.path()).model, "test-model");
}

#[test]
fn plain_http_only_for_loopback_hosts() {
    assert!(organizer::validate_settings(&settings("http://[::1]:11434/v1".into())).is_ok());
    assert!(organizer::validate_settings(&settings("http://localhost:11434/v1".into())).is_ok());
    assert!(organizer::validate_settings(&settings("http://api.example.com/v1".into())).is_err());
}

#[test]
fn organizer_calls_openai_compatible_endpoint_and_validates_sources() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // 按 Content-Length 读全请求：macOS 回环网卡单次 read 经常只交付
        // 头部 + 部分 body，一次 read 就断言会把 "model" 误判为缺失。
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0, "client closed before sending the full request");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let body_len = head.lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(str::to_string))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < header_end + body_len {
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0, "client closed mid-body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let request = String::from_utf8_lossy(&buf).to_string();
        tx.send(request).unwrap();
        let content = json!({
            "overview": "今天推进了测试项目",
            "projects": [{
                "project": "demo",
                "status": "in_progress",
                "summary": "完成整理接口，仍需验证界面",
                "completed": ["完成整理接口"],
                "in_progress": ["验证界面"],
                "blocked": [],
                "decisions": ["保持来源可追溯"],
                "next_steps": ["运行前端测试"],
                "sources": ["codex:s1"]
            }]
        }).to_string();
        let body = json!({"choices":[{"message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":123,"completion_tokens":45,"total_tokens":168}}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let activity = json!({
        "day": "2026-09-08",
        "project_activity": [{
            "project": "demo",
            "activities": [{"session_id":"codex:s1","agent":"codex","title":"实现整理","tail":"接口已完成"}],
            "open_tasks": (0..30).map(|i| json!({"content": format!("很长的任务 {i} {}", "x".repeat(500))})).collect::<Vec<_>>(),
            "raw_transcript": "FULL_TRANSCRIPT_MUST_NOT_LEAVE_DEVICE"
        }]
    });
    let mut api_settings = settings(format!("http://{addr}/v1"));
    api_settings.max_input_chars = 4_000;
    let out = organizer::organize_with_api(
        &api_settings,
        "test-secret",
        &activity,
    ).unwrap();
    assert_eq!(out["result"]["projects"][0]["sources"][0], "codex:s1");
    assert_eq!(out["usage"]["total_tokens"], 168);
    assert!(out["input_chars"].as_u64().unwrap() <= 4_000);
    assert_eq!(out["truncated"], true);
    let request = rx.recv().unwrap();
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(request.to_ascii_lowercase().contains("authorization: bearer test-secret"));
    assert!(request.contains("test-model"));
    assert!(!request.contains("FULL_TRANSCRIPT_MUST_NOT_LEAVE_DEVICE"));
}

#[test]
fn saving_a_reviewed_project_summary_creates_confirmed_context_memory() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let (pid, _) = db::add_project(&conn, "/tmp/demo").unwrap();
    conn.execute(
        "INSERT INTO sessions(id,agent,native_id,project_id,file_path,started_at,ended_at,message_count,created_at,updated_at)
         VALUES ('codex:s1','codex','s1',?1,'/tmp/s1.jsonl','2026-09-08T01:00:00Z','2026-09-08T02:00:00Z',2,'2026-09-08T01:00:00Z','2026-09-08T02:00:00Z')",
        [pid],
    ).unwrap();
    let summary = json!({
        "project": "demo", "status": "in_progress", "summary": "整理接口已完成",
        "completed": ["完成接口"], "in_progress": ["验证界面"], "blocked": [],
        "decisions": ["保留来源"], "next_steps": ["运行测试"], "sources": ["codex:s1"]
    });
    let saved = organizer::save_project_summary(&conn, "2026-09-08", &summary).unwrap();
    assert_eq!(saved["status"], "confirmed");
    let (content, status, kind): (String, String, String) = conn.query_row(
        "SELECT content,status,type FROM memories WHERE id=?1", [saved["id"].as_str().unwrap()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    ).unwrap();
    assert_eq!(status, "confirmed");
    assert_eq!(kind, "context");
    assert!(content.contains("整理接口已完成"));
    assert!(content.contains("codex:s1"));
}

#[test]
fn project_activity_is_bounded_and_contains_only_summary_fields() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    let (pid, _) = db::add_project(&conn, "/tmp/research").unwrap();
    for i in 0..101 {
        conn.execute(
            "INSERT INTO sessions(id,agent,native_id,project_id,file_path,started_at,ended_at,message_count,created_at,updated_at)
             VALUES (?1,'codex',?2,?3,?4,'2026-10-24T01:00:00Z','2026-10-24T02:00:00Z',2,'2026-10-24T01:00:00Z','2026-10-24T02:00:00Z')",
            rusqlite::params![format!("codex:s{i:03}"), format!("s{i:03}"), pid, format!("/tmp/s{i}.jsonl")],
        ).unwrap();
    }
    conn.execute(
        "INSERT INTO messages(session_id,line_no,ord,kind,content) VALUES
         ('codex:s100',1,0,'user',?1),
         ('codex:s100',2,0,'assistant',?2),
         ('codex:s100',3,0,'tool_result','FULL_TOOL_OUTPUT_MUST_NOT_BE_SENT')",
        rusqlite::params![format!("研究方法 {}", "x".repeat(300)) + "FULL_USER_SECRET", format!("分析结论 {}", "y".repeat(1800)) + "FULL_ASSISTANT_SECRET"],
    ).unwrap();
    let activity = dossier::project_activity(&conn, pid).unwrap();
    let project = &activity["project_activity"][0];
    assert_eq!(project["activities"].as_array().unwrap().len(), 100);
    assert_eq!(activity["truncated"], true);
    let serialized = activity.to_string();
    assert!(!serialized.contains("FULL_USER_SECRET"));
    assert!(!serialized.contains("FULL_ASSISTANT_SECRET"));
    assert!(!serialized.contains("FULL_TOOL_OUTPUT_MUST_NOT_BE_SENT"));
    assert_eq!(project["activities"][0]["session_id"], "codex:s100");
}
