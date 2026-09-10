//! pi adapter——v3 树状 JSONL 解析：session 头行 cwd、role 分流
//! （user / assistant 分条目 / toolResult 独立 role）、write/edit artifact、
//! 防御式、文件名派生身份、增量导入。
//! 样本全合成（隐私纪律）：结构按真机 56 文件解剖逐型保真，内容不含真实会话。

use serde_json::json;
use yourmem::adapters::{self, pi};
use yourmem::{db, ingest};

use std::path::{Path, PathBuf};

fn lines(v: &[serde_json::Value]) -> Vec<(u64, String)> {
    v.iter().enumerate().map(|(i, j)| ((i + 1) as u64, j.to_string())).collect()
}

/// 覆盖全部行级类型与三种 role 的合成样本（与真机词表一一对应）。
fn sample() -> Vec<serde_json::Value> {
    vec![
        json!({"type":"session","version":3,"id":"00000000-0000-0000-0000-00000000000a",
               "timestamp":"2026-09-01T10:00:00.000Z","cwd":"/Users/test/pi-proj"}),
        json!({"type":"model_change","id":"aaaaaaaa","parentId":null,
               "timestamp":"2026-09-01T10:00:00.100Z","provider":"synthetic","modelId":"synthetic-model"}),
        json!({"type":"thinking_level_change","id":"aaaaaaab","parentId":"aaaaaaaa",
               "timestamp":"2026-09-01T10:00:00.100Z","thinkingLevel":"high"}),
        json!({"type":"message","id":"aaaaaaac","parentId":"aaaaaaab","timestamp":"2026-09-01T10:00:01.000Z",
               "message":{"role":"user","content":[{"type":"text","text":"帮我建一份示例清单"}]}}),
        // 一行 assistant 承载 thinking + 双 toolCall + text：四个 NewMessage 共 line_no
        json!({"type":"message","id":"aaaaaaad","parentId":"aaaaaaac","timestamp":"2026-09-01T10:00:02.000Z",
               "message":{"role":"assistant","content":[
                   {"type":"thinking","thinking":"先写文件再回复"},
                   {"type":"toolCall","id":"call_1","name":"write","arguments":{"path":"list.md","content":"# 清单"}},
                   {"type":"toolCall","id":"call_2","name":"edit","arguments":{"path":"notes.md","edits":[{"oldText":"# 清单","newText":"# 清单 v2"}]}},
                   {"type":"text","text":"已建好示例清单"}
               ]}}),
        json!({"type":"message","id":"aaaaaaae","parentId":"aaaaaaad","timestamp":"2026-09-01T10:00:03.000Z",
               "message":{"role":"toolResult","toolCallId":"call_1","toolName":"write",
                          "content":[{"type":"text","text":"wrote list.md"}],"details":{},"isError":false}}),
        // 只含未知条目（image）的消息：零入账
        json!({"type":"message","id":"aaaaaaaf","parentId":"aaaaaaae","timestamp":"2026-09-01T10:00:04.000Z",
               "message":{"role":"assistant","content":[{"type":"image","url":"data:image/png;base64,xxx"}]}}),
    ]
}

#[test]
fn temp_buckets_are_excluded_at_discovery() {
    // 真机形态：pi 会话桶 = `--` 包裹的 munge(cwd)；临时目录下的 scratchpad 整桶不采
    let temp = Path::new("C:\\Users\\test\\AppData\\Local\\Temp");
    let files = vec![
        // 真实项目桶：保留
        PathBuf::from("/r/--D--work-real--/a.jsonl"),
        // temp 根本身 / temp 下 scratchpad：排除
        PathBuf::from("/r/--C--Users-test-AppData-Local-Temp--/b.jsonl"),
        PathBuf::from("/r/--C--Users-test-AppData-Local-Temp-claude-D--work-x-scratchpad--/c.jsonl"),
        // 前缀相似但不属于 temp 的桶：保留（前缀后必须跟分隔符 `-`）
        PathBuf::from("/r/--C--Users-test-AppData-Local-Templates--/d.jsonl"),
        // 非 munge 桶形态（extra_roots 直挂文件等）：照常采集
        PathBuf::from("/r/plain.jsonl"),
        PathBuf::from("/r/not-a-bucket/e.jsonl"),
    ];
    let kept = pi::exclude_temp_buckets_with(temp, files);
    assert_eq!(
        kept,
        vec![
            PathBuf::from("/r/--D--work-real--/a.jsonl"),
            PathBuf::from("/r/--C--Users-test-AppData-Local-Templates--/d.jsonl"),
            PathBuf::from("/r/plain.jsonl"),
            PathBuf::from("/r/not-a-bucket/e.jsonl"),
        ],
        "temp 本身与子路径排除，相似前缀与直挂文件保留"
    );

    // 大小写不一致仍命中（Windows 路径大小写不敏感，TEMP 环境值与桶名大小写可能不同）
    let one = vec![PathBuf::from("/r/--c--users-test-appdata-local-temp--/f.jsonl")];
    assert!(pi::exclude_temp_buckets_with(temp, one).is_empty());
    // 剥掉 temp 根尾部分隔符（GetTempPathW 带尾杠的环境差异）
    let one = vec![PathBuf::from("/r/--C--Users-test-AppData-Local-Temp--/g.jsonl")];
    assert!(pi::exclude_temp_buckets_with(Path::new("C:\\Users\\test\\AppData\\Local\\Temp\\"), one).is_empty());
}

#[test]
fn parses_synthetic_sample() {
    let out = pi::parse_lines(&lines(&sample()));
    let summary: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();

    // 总账：user 1 + thinking 1 + assistant 1 + toolCall 2 + toolResult 1 = 6
    assert_eq!(out.messages.len(), 6, "{summary:?}");
    assert!(summary.iter().any(|(k, c)| *k == "user" && *c == "帮我建一份示例清单"));
    assert!(summary.iter().any(|(k, c)| *k == "thinking" && *c == "先写文件再回复"));
    assert!(summary.iter().any(|(k, c)| *k == "assistant" && *c == "已建好示例清单"));
    // toolCall 带 [name] args-json；toolResult 前缀用工具名（可搜），非不透明 call id
    assert!(summary.iter().any(|(k, c)| *k == "tool_call" && c.starts_with("[write] {")));
    assert!(summary.iter().any(|(k, c)| *k == "tool_call" && c.starts_with("[edit] {")));
    assert!(summary.iter().any(|(k, c)| *k == "tool_result" && *c == "[write] wrote list.md"));
    assert!(!summary.iter().any(|(_, c)| c.contains("base64")), "image 条目不入账");
    // write + edit 都按 arguments.path 提取 artifact
    assert_eq!(out.artifacts.len(), 2, "{:?}", out.artifacts);
    assert_eq!((out.artifacts[0].tool.as_str(), out.artifacts[1].tool.as_str()), ("write", "edit"));
    // cwd 与时间窗：窗口按全部 message 行计（含只含未知条目的未入账行）
    assert_eq!(out.meta.cwd.as_deref(), Some("/Users/test/pi-proj"));
    assert_eq!(out.meta.started_at.as_deref(), Some("2026-09-01T10:00:00.000Z"));
    assert_eq!(out.meta.ended_at.as_deref(), Some("2026-09-01T10:00:04.000Z"));
}

#[test]
fn edge_cases_bad_lines_and_unknown_types() {
    let v = vec![
        json!({ "损坏": true }), // 无 type 字段：走未知类型跳过
        json!({"type":"message","id":"b1","timestamp":"2026-09-01T10:00:00.000Z"}),
        json!({"type":"message","id":"b2","timestamp":"2026-09-01T10:00:01.000Z",
               "message":{"role":"reviewer","content":[{"type":"text","text":"未知角色不入账"}]}}),
        json!({"type":"compaction","id":"b3","timestamp":"2026-09-01T10:00:02.000Z","note":"未来类型跳过"}),
        json!({"type":"message","id":"b4","timestamp":"2026-09-01T10:00:03.000Z",
               "message":{"role":"user","content":[{"type":"text","text":"   "}]}}), // 空白文本不入账
        json!({"type":"message","id":"b5","timestamp":"2026-09-01T10:00:04.000Z",
               "message":{"role":"user","content":[{"type":"text","text":"有效提问"}]}}),
    ];
    let mut ls: Vec<(u64, String)> = vec![(1, "{ 损坏行".to_string())];
    ls.extend(lines(&v));
    let out = pi::parse_lines(&ls);
    let summary: Vec<(&str, &str)> = out.messages.iter().map(|m| (m.kind.as_str(), m.content.as_str())).collect();
    assert_eq!(out.messages.len(), 1, "{summary:?}");
    assert_eq!(summary[0], ("user", "有效提问"));
    // 时间窗按全部 message 行计：缺体/未知角色/空白行的时间戳都算活动
    assert_eq!(out.meta.started_at.as_deref(), Some("2026-09-01T10:00:00.000Z"));
}

#[test]
fn identity_from_filename_and_resume() {
    let p = std::path::Path::new("/sessions/--D--work-x--/2026-09-01T10-00-00-000Z_00000000-0000-0000-0000-00000000000a.jsonl");
    assert_eq!(adapters::native_id("pi", p), "00000000-0000-0000-0000-00000000000a");
    assert_eq!(adapters::session_key("pi", "00000000-0000-0000-0000-00000000000a"), "pi:00000000-0000-0000-0000-00000000000a");
    assert_eq!(
        adapters::resume_command("pi", "00000000-0000-0000-0000-00000000000a").as_deref(),
        Some("pi --session 00000000-0000-0000-0000-00000000000a")
    );
    // 非典型文件名：退化为 stem，永不 panic
    assert_eq!(adapters::native_id("pi", std::path::Path::new("plain.jsonl")), "plain");
}

#[test]
fn imports_session_dir_and_assigns_project() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("--Users-test-pi-proj--");
    std::fs::create_dir_all(&dir).unwrap();
    let raw: String = sample().iter().map(|j| format!("{j}\n")).collect();
    std::fs::write(dir.join("2026-09-01T10-00-00-000Z_00000000-0000-0000-0000-00000000000a.jsonl"), raw).unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_PI, root.path().to_path_buf())];
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out.messages_added, 6);

    let (cwd, count, pid_path): (Option<String>, i64, Option<String>) = conn.query_row(
        "SELECT s.cwd, s.message_count, p.path FROM sessions s
         LEFT JOIN projects p ON p.id = s.project_id WHERE s.id = 'pi:00000000-0000-0000-0000-00000000000a'",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!(cwd.as_deref(), Some("/Users/test/pi-proj"), "cwd 取自会话头行");
    assert_eq!(count, 6);
    assert_eq!(pid_path.as_deref(), Some("/Users/test/pi-proj"), "有 cwd 即归属项目");
    let art: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_artifacts WHERE session_id = 'pi:00000000-0000-0000-0000-00000000000a'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(art, 2, "write + edit 各一个 artifact");
}

#[test]
fn incremental_append_only_imports_new_lines() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("--tmp--");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("2026-09-01T10-00-00-000Z_00000000-0000-0000-0000-00000000000b.jsonl");
    let head: String = sample()[..4].iter().map(|j| format!("{j}\n")).collect();
    std::fs::write(&f, head).unwrap();

    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_PI, root.path().to_path_buf())];
    let out1 = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out1.messages_added, 1, "头行 + 一条 user 消息");

    // 追加一条 assistant 消息 → 只入账新行
    let tail = json!({"type":"message","id":"c1","parentId":null,"timestamp":"2026-09-01T10:05:00.000Z",
                      "message":{"role":"assistant","content":[{"type":"text","text":"增量回复"}]}});
    use std::io::Write;
    let mut fh = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    writeln!(fh, "{tail}").unwrap();
    drop(fh);
    let out2 = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    assert_eq!(out2.messages_added, 1, "只补采追加行");
    let count: i64 = conn.query_row(
        "SELECT message_count FROM sessions WHERE id = 'pi:00000000-0000-0000-0000-00000000000b'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(count, 2);
}
