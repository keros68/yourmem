use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

use serde_json::json;
use yourmem::{db, organizer};

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
fn organizer_calls_openai_compatible_endpoint_and_validates_sources() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 32_768];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
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
