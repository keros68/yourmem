//! Antigravity CLI adapter：只采 transcript_full.jsonl、step 分流、args 解码、
//! 压缩点、history.jsonl 补 cwd。fixture 全合成。

use serde_json::json;
use yourmem::adapters::{self, antigravity};
use yourmem::{db, ingest};

fn lines() -> Vec<String> {
    vec![
        json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-09-15T12:00:00Z",
            "content":"<USER_REQUEST>\n整理临江市内涝报告\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal time\n</ADDITIONAL_METADATA>"}),
        json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-09-15T12:00:05Z",
            "thinking":"先看目录","tool_calls":[
                {"name":"run_command","args":{"CommandLine":"\"ls\"","Cwd":"\"/tmp/ag-proj\""}},
                {"name":"write_to_file","args":{"TargetFile":"\"/tmp/ag-proj/report.md\"","CodeContent":"\"# 报告\""}}]}),
        json!({"step_index":2,"source":"MODEL","type":"GENERIC","created_at":"2026-09-15T12:00:06Z","content":"report.md"}),
        "{ 损坏的行".to_string().into(),
        json!({"step_index":3,"source":"SYSTEM","type":"CHECKPOINT","created_at":"2026-09-15T12:10:00Z",
            "content":"{{ CHECKPOINT 0 }} 摘要"}),
        json!({"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-09-15T12:10:05Z","content":"已完成"}),
        json!({"step_index":5,"source":"SYSTEM","type":"FUTURE_TYPE","created_at":"2026-09-15T12:10:06Z","content":"跳过"}),
    ]
    .into_iter()
    .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
    .collect()
}

#[test]
fn parses_steps_args_and_checkpoint() {
    let input: Vec<(u64, String)> = lines().into_iter().enumerate().map(|(i, l)| (i as u64 + 1, l)).collect();
    let out = antigravity::parse_lines(&input);
    let got: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();
    assert_eq!(got[0], ("user", "整理临江市内涝报告"), "只取 USER_REQUEST 本体");
    assert_eq!(got[1], ("thinking", "先看目录"));
    assert!(got[2].1.starts_with("[run_command] ") && got[2].1.contains("\"Cwd\":\"/tmp/ag-proj\""), "{got:?}");
    assert_eq!(got[4], ("tool_result", "report.md"));
    assert_eq!(got[5].0, "summary");
    assert_eq!(got[6], ("assistant", "已完成"));
    assert_eq!(got.len(), 7, "损坏行与未知类型跳过: {got:?}");
    assert_eq!(out.compact_line, Some(5));
    assert_eq!(out.meta.cwd.as_deref(), Some("/tmp/ag-proj"));
    assert_eq!(out.artifacts.len(), 1);
    assert_eq!(out.artifacts[0].path, "/tmp/ag-proj/report.md");
    assert_eq!(out.meta.started_at.as_deref(), Some("2026-09-15T12:00:00Z"));
    assert_eq!(out.meta.ended_at.as_deref(), Some("2026-09-15T12:10:06Z"));
}

#[test]
fn imports_only_full_transcript_with_history_workspace() {
    let cli = tempfile::tempdir().unwrap();
    let id = "0b5c6d1e-0000-4000-8000-000000000001";
    let logs = cli.path().join("brain").join(id).join(".system_generated").join("logs");
    std::fs::create_dir_all(logs.join("chunks").join("transcript_full")).unwrap();
    let body = lines().join("\n") + "\n";
    std::fs::write(logs.join("transcript_full.jsonl"), &body).unwrap();
    // 截断版与分片是同一会话的副本，不得重复采集
    std::fs::write(logs.join("transcript.jsonl"), &body).unwrap();
    std::fs::write(logs.join("chunks").join("transcript_full").join("00000000.jsonl"), &body).unwrap();
    std::fs::write(
        cli.path().join("history.jsonl"),
        json!({"display":"整理","timestamp":1,"workspace":"/tmp/ag-launch","conversationId":id}).to_string() + "\n",
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let mut conn = db::open(home.path()).unwrap();
    ingest::import_all(&mut conn, home.path(), &[(adapters::AGENT_ANTIGRAVITY, cli.path().join("brain"))], None, None)
        .unwrap();
    let rows: Vec<(String, Option<String>, i64)> = conn
        .prepare("SELECT id, cwd, message_count FROM sessions")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("antigravity:{id}"));
    assert_eq!(rows[0].1.as_deref(), Some("/tmp/ag-launch"), "启动目录优先于 run_command 的 Cwd");
    assert_eq!(rows[0].2, 7);
}
