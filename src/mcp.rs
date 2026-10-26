//! MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Kept deliberately small: initialize, ping, tools/list, tools/call.
//! Agents launch `yourmem mcp`; the server's cwd is inherited from the
//! agent process, which is how project auto-detection works.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::db::{self, HandoffFields, SearchOpts};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// get_project_context 的增量游标（多窗口共享看板裁定：每个窗口记自己上次
/// 看到的版本，只补"自你上次读取以来的变化"）。一个 MCP 进程对应一个 agent
/// 窗口，游标按 (home, project) 存进程内存——窗口重开即重新全量读取，
/// 不持久化也不跨窗口共享，避免一个窗口的读取吃掉另一个窗口的增量。
fn context_cursors() -> &'static Mutex<HashMap<(String, i64), String>> {
    static CURSORS: OnceLock<Mutex<HashMap<(String, i64), String>>> = OnceLock::new();
    CURSORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn serve(home: &Path) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(
                    &mut out,
                    &error(Value::Null, -32700, &format!("parse error: {e}")),
                )?;
                continue;
            }
        };
        // 回复 initialize 前先在后台起一次增量导入：agent 的连接超时只有几秒，
        // 首次全量导入可能要几分钟，不能挡在握手前面
        if req.get("method").and_then(Value::as_str) == Some("initialize") {
            let home = home.to_path_buf();
            std::thread::spawn(move || init_import(&home));
        }
        if let Some(resp) = handle(home, &req) {
            write_msg(&mut out, &resp)?;
        }
    }
    Ok(())
}

fn write_msg(out: &mut impl Write, msg: &Value) -> anyhow::Result<()> {
    out.write_all(serde_json::to_string(msg)?.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// Handle one request. Returns `None` for notifications.
pub fn handle(home: &Path, req: &Value) -> Option<Value> {
    let has_id = req.get("id").is_some();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    // JSON-RPC 边缘（自检 C2）：带 id 却无 method 是无效请求，回 -32600 而非
    // 静默丢弃；notification（无 id）一律不回包——包括 ping/tools/call 的通知形态
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return if has_id {
            Some(error(id, -32600, "invalid request: missing method"))
        } else {
            None
        };
    };

    match method {
        "notifications/initialized" | "notifications/cancelled" => None,
        _ if !has_id => None,
        "initialize" => Some(result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "yourmem", "version": env!("CARGO_PKG_VERSION") },
            }),
        )),
        "ping" => Some(result(id, json!({}))),
        "tools/list" => Some(result(id, tools_list())),
        "tools/call" => Some(call_tool(home, id, &req["params"])),
        _ => Some(error(id, -32601, "method not found")),
    }
}

fn result(id: Value, r: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": r })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn ok_text(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] })
}

fn err_text(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

/// initialize 时在后台增量导入（消费入口自带新鲜度）：尽力而为，失败只记 stderr 不传导；`.last_import` 60 秒单飞（watch/CLI/app 同钟）。
pub fn init_import(home: &Path) {
    let fresh = std::fs::metadata(home.join(".last_import"))
        .and_then(|m| m.modified())
        .is_ok_and(|t| match t.elapsed() {
            Ok(d) => d < std::time::Duration::from_secs(60),
            // 未来 mtime（时钟回拨/NTP 校正）：按不新鲜处理，宁可多导一次
            Err(_) => false,
        });
    if fresh {
        return;
    }
    if let Err(e) = crate::ingest::import_defaults(home)
        .and_then(|_| db::log_usage(&db::open(home)?, "mcp", "import_on_init"))
    {
        eprintln!("yourmem mcp: import_on_init failed (continuing anyway): {e:#}");
    }
}

fn call_tool(home: &Path, id: Value, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let outcome = run_tool(home, name, &args);
    let count = outcome
        .as_ref()
        .ok()
        .and_then(|v| v["results"].as_array().or_else(|| v["messages"].as_array()))
        .map(|v| v.len() as u64);
    crate::recall_status::record(home, "mcp", name, outcome.is_ok(), count);
    match outcome {
        Ok(v) => result(id, ok_text(v)),
        Err(e) => result(id, err_text(&format!("{e:#}"))),
    }
}

fn run_tool(home: &Path, name: &str, args: &Value) -> anyhow::Result<Value> {
    // 只重试只读工具：写入工具不全是单事务，中途失败后重试会产生重复记录；写入靠 busy_timeout 等锁
    let read_only = ["search_", "read_", "get_", "list_"]
        .iter()
        .any(|p| name.starts_with(p));
    if !read_only {
        return run_tool_once(home, name, args);
    }
    let mut last = None;
    for attempt in 0..3 {
        match run_tool_once(home, name, args) {
            Ok(value) => return Ok(value),
            Err(error)
                if format!("{error:#}")
                    .to_lowercase()
                    .contains("database is locked")
                    || format!("{error:#}")
                        .to_lowercase()
                        .contains("database busy") =>
            {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1)));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("database busy")))
}

fn run_tool_once(home: &Path, name: &str, args: &Value) -> anyhow::Result<Value> {
    let conn = db::open(home)?;
    let _ = db::log_usage(&conn, "mcp", name); // 本地指标，失败静默
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let arg_str = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);

    match name {
        "list_projects" => Ok(json!({ "projects": db::list_projects(&conn)? })),

        "get_project_context" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            // 先取时间再查询：查询窗口内落下的写入会在下次增量里再报（宁多勿漏）
            let generated_at = crate::now_iso();
            let mut context = db::project_context(&conn, pid)?;
            context["source_review"] = crate::project_review::status(&conn, home, pid)?;
            context["project_docs"] = crate::project_docs::attach(&conn, home, pid);
            context["project_records"] = json!(crate::work::project_records(&conn, pid)?);
            context["freshness"] = crate::work::freshness(home);
            context["generated_at"] = json!(generated_at);
            let recent_count = context["recent_sessions"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0);
            let memory_count = context["memories"]["confirmed"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0);
            context["pagination"] = json!({
                "recent_sessions":{"returned_count":recent_count,"truncated":recent_count==5,"continuation_tool":"get_timeline"},
                "confirmed_memories":{"returned_count":memory_count,"truncated":memory_count==20,"continuation_tool":"search_memory"},
                "delta_protocol":"get_changes(database_generation + seq); legacy since_last_read 仅为兼容"
            });
            let key = (home.to_string_lossy().to_string(), pid);
            let mut cursors = context_cursors().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(since) = cursors.get(&key).cloned() {
                context["since_last_read"] = db::changes_since(&conn, pid, &since)?;
            }
            cursors.insert(key, generated_at);
            Ok(context)
        }

        "get_dossier" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let mut d = crate::dossier::project_dossier(&conn, pid)?;
            d["project_docs"] = crate::project_docs::attach(&conn, home, pid);
            Ok(d)
        }

        "search_history" => {
            let query = arg_str("query")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
            let hits = db::search(
                &conn,
                &SearchOpts {
                    query,
                    project: arg_str("project"),
                    agent: arg_str("agent"),
                    kind: arg_str("kind"),
                    limit: args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32,
                },
            )?;
            Ok(json!({ "results": hits }))
        }

        "read_session" => {
            let sid = arg_str("session_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: session_id"))?;
            let max = args
                .get("max_messages")
                .and_then(Value::as_u64)
                .unwrap_or(60) as u32;
            let line = args.get("line").and_then(Value::as_i64);
            let before_compact = args
                .get("before_compact")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            db::read_session(&conn, &sid, max, line, before_compact)
        }

        "read_native_memory" => {
            if let Some(file_id) = args.get("file_id").and_then(Value::as_i64) {
                crate::memfiles::show(
                    &conn,
                    home,
                    file_id,
                    args.get("revision").and_then(Value::as_i64),
                )
            } else {
                let agent = arg_str("agent").ok_or_else(|| {
                    anyhow::anyhow!("missing required argument: agent (or file_id)")
                })?;
                crate::memfiles::read_native(&conn, home, &agent, arg_str("path").as_deref())
            }
        }

        "create_handoff" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let session_id = arg_str("session_id");
            let fields = HandoffFields {
                title: &arg_str("title").unwrap_or_default(),
                done: &arg_str("done").unwrap_or_default(),
                state: &arg_str("state").unwrap_or_default(),
                decisions: &arg_str("decisions").unwrap_or_default(),
                files_changed: &arg_str("files_changed").unwrap_or_default(),
                open_issues: &arg_str("open_issues").unwrap_or_default(),
                next_steps: &arg_str("next_steps").unwrap_or_default(),
                session_id: session_id.as_deref(),
            };
            let task_id = match arg_str("task_id") {
                Some(ident) => Some(crate::work::resolve_task(&conn, &ident, Some(pid))?),
                None => None,
            };
            let saved = crate::work::create_handoff(&conn, pid, &fields, task_id.as_deref())?;
            let (name, path) = conn
                .query_row(
                    "SELECT name, path FROM projects WHERE id = ?1",
                    rusqlite::params![pid],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .unwrap_or_default();
            Ok(
                json!({ "handoff_id": saved["handoff_id"], "uuid":saved["uuid"], "task_id":task_id, "project": name, "project_path": path }),
            )
        }

        "get_recent_work" => {
            let pid = db::optional_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32;
            let sessions = db::recent_sessions(&conn, pid, limit)?;
            let handoff = match pid {
                Some(pid) => db::latest_handoff(&conn, pid)?,
                None => None,
            };
            let tasks = db::open_tasks(&conn, pid)?;
            Ok(
                json!({ "recent_sessions": sessions, "latest_handoff": handoff, "open_tasks": tasks }),
            )
        }

        "list_tasks" => {
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            Ok(
                json!({"tasks":crate::work::list_tasks(&conn,pid,arg_str("status").as_deref(),arg_str("agent").as_deref(),args.get("limit").and_then(Value::as_i64).unwrap_or(50))?}),
            )
        }

        "create_task" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let title = arg_str("title")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: title"))?;
            crate::work::create_task(
                &conn,
                pid,
                &title,
                arg_str("session_id").as_deref(),
                args.get("allow_duplicates")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )
        }

        "get_task_context" => {
            let ident = arg_str("task_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: task_id"))?;
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let task_id = crate::work::resolve_task(&conn, &ident, pid)?;
            crate::work::task_context(
                &conn,
                home,
                &task_id,
                args.get("limit").and_then(Value::as_i64).unwrap_or(50),
            )
        }

        "update_task" => {
            let ident = arg_str("task_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: task_id"))?;
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let task_id = crate::work::resolve_task(&conn, &ident, pid)?;
            crate::work::update_task(
                &conn,
                &task_id,
                arg_str("title").as_deref(),
                arg_str("status").as_deref(),
                args.get("expected_revision").and_then(Value::as_i64),
                arg_str("session_id").as_deref(),
                false,
            )
        }

        "link_task" => {
            let ident = arg_str("task_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: task_id"))?;
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let task_id = crate::work::resolve_task(&conn, &ident, pid)?;
            let relation = arg_str("relation").unwrap_or_else(|| "contribution".into());
            match arg_str("session_id") {
                Some(sid) => {
                    crate::work::link_session(&conn, &task_id, &sid, &relation, "agent_declared")
                }
                None => crate::work::issue_claim(&conn, &task_id, &relation),
            }
        }

        "unlink_task" => {
            let ident = arg_str("task_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: task_id"))?;
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let task_id = crate::work::resolve_task(&conn, &ident, pid)?;
            let sid = arg_str("session_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: session_id"))?;
            crate::work::unlink_session(&conn, &task_id, &sid)
        }

        "close_work" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let ident = arg_str("task_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: task_id"))?;
            let task_id = crate::work::resolve_task(&conn, &ident, Some(pid))?;
            let session_id = arg_str("session_id");
            let fields = HandoffFields {
                title: &arg_str("title").unwrap_or_default(),
                done: &arg_str("done").unwrap_or_default(),
                state: &arg_str("state").unwrap_or_default(),
                decisions: &arg_str("decisions").unwrap_or_default(),
                files_changed: &arg_str("files_changed").unwrap_or_default(),
                open_issues: &arg_str("open_issues").unwrap_or_default(),
                next_steps: &arg_str("next_steps").unwrap_or_default(),
                session_id: session_id.as_deref(),
            };
            crate::work::close_work(&conn, pid, &task_id, &fields, arg_str("status").as_deref())
        }

        "add_task_verification" => {
            let ident = arg_str("task_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: task_id"))?;
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let task_id = crate::work::resolve_task(&conn, &ident, pid)?;
            crate::work::add_verification(
                &conn,
                &task_id,
                arg_str("kind").as_deref().unwrap_or("test"),
                arg_str("result").as_deref().unwrap_or("unknown"),
                args.get("source_message_id").and_then(Value::as_i64),
                arg_str("evidence").as_deref().unwrap_or(""),
                false,
            )
        }

        "get_timeline" => {
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let task_id = match arg_str("task_id") {
                Some(ident) => Some(crate::work::resolve_task(&conn, &ident, pid)?),
                None => None,
            };
            let kinds: Vec<String> = args
                .get("kinds")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            crate::work::timeline(
                &conn,
                pid,
                task_id.as_deref(),
                arg_str("from").as_deref(),
                arg_str("to").as_deref(),
                &kinds,
                arg_str("cursor").as_deref(),
                args.get("limit").and_then(Value::as_i64).unwrap_or(50),
            )
        }

        "get_changes" => {
            let cursor = args.get("cursor").cloned().unwrap_or_else(|| json!({}));
            crate::work::changes(
                &conn,
                cursor.get("database_generation").and_then(Value::as_str),
                cursor.get("seq").and_then(Value::as_i64).unwrap_or(0),
                args.get("limit").and_then(Value::as_i64).unwrap_or(100),
            )
        }

        "list_project_records" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            Ok(
                json!({"records":crate::work::project_records(&conn,pid)?,"freshness":crate::work::freshness(home)}),
            )
        }

        "get_project_record" => {
            let (pid, ..) =
                db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let record_id = arg_str("record_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: record_id"))?;
            crate::work::get_project_record(&conn, home, pid, &record_id)
        }

        "search_work" => {
            let query = arg_str("query")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
            let pid = db::cwd_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            Ok(
                json!({"results":crate::work::search_work(&conn,&query,pid,args.get("limit").and_then(Value::as_u64).unwrap_or(20) as u32)?}),
            )
        }

        "save_memory" => {
            let content = arg_str("content")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: content"))?;
            let r#type = arg_str("type").unwrap_or_else(|| "fact".to_string());
            let scope = arg_str("scope").unwrap_or_else(|| "project".to_string());
            let pid = if scope == "global" {
                None
            } else {
                Some(db::require_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?.0)
            };
            let source_session = arg_str("session_id");
            // 写路径纪律（engramory 吸收）：查重提示随结果返回，agent 据 此
            // 判断"更新优于复制"（知情门控，不拦截）
            let (id, similar) = db::save_memory_with_similar(
                &conn,
                &db::MemoryInput {
                    project_id: pid,
                    scope: &scope,
                    r#type: &r#type,
                    content: &content,
                    status: arg_str("status").as_deref(),
                    source_session_id: source_session.as_deref(),
                    source_message_id: args.get("message_id").and_then(Value::as_i64),
                },
            )?;
            Ok(json!({
                "memory_id": id,
                "similar": similar,
                "note": if similar.is_empty() { Value::Null } else {
                    json!("检测到疑似重复：更新优于复制——若语义重复，用 update_memory supersede 旧条或请用户确认后再保留两条")
                },
                "promote_hint": db::promotion_hint(&r#type, &similar),
            }))
        }

        "search_memory" => {
            let query = arg_str("query")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
            let pid = db::optional_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let mems = db::search_memory(
                &conn,
                &query,
                &db::MemoryFilter {
                    project_id: pid,
                    scope: arg_str("scope"),
                    r#type: arg_str("type"),
                    status: arg_str("status"),
                    agent: arg_str("agent"),
                    include_global: pid.is_some(),
                    limit: args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32,
                },
            )?;
            Ok(json!({ "memories": mems }))
        }

        "list_memories" => {
            let pid = db::optional_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let mems = db::list_memories(
                &conn,
                &db::MemoryFilter {
                    project_id: pid,
                    scope: arg_str("scope"),
                    r#type: arg_str("type"),
                    status: arg_str("status"),
                    agent: arg_str("agent"),
                    include_global: pid.is_some(),
                    limit: args.get("limit").and_then(Value::as_u64).unwrap_or(50) as u32,
                },
            )?;
            Ok(json!({ "memories": mems }))
        }

        "update_memory" => {
            let id =
                arg_str("id").ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let action = arg_str("action").ok_or_else(|| {
                anyhow::anyhow!("missing required argument: action (confirm|archive|supersede)")
            })?;
            let by = arg_str("superseded_by");
            db::update_memory_status_at(
                &conn,
                &id,
                &action,
                by.as_deref(),
                arg_str("expected_updated_at").as_deref(),
            )?;
            Ok(json!({ "memory_id": id, "action": action }))
        }

        "list_artifacts" => {
            let pid = db::optional_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?;
            let session = arg_str("session_id");
            let arts = db::list_artifacts(
                &conn,
                pid,
                session.as_deref(),
                args.get("limit").and_then(Value::as_u64).unwrap_or(50) as u32,
            )?;
            Ok(json!({ "artifacts": arts }))
        }

        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    }
}

fn tools_list() -> Value {
    let project = json!({"type":"string"});
    let text = json!({"type":"string"});
    let number = json!({"type":"number"});
    let boolean = json!({"type":"boolean"});
    let schema = |properties: Value, required: &[&str]| {
        if required.is_empty() {
            json!({"type":"object","properties":properties})
        } else {
            json!({"type":"object","properties":properties,"required":required})
        }
    };
    json!({"tools":[
        {"name":"list_projects","description":"List projects and activity counts.","inputSchema":schema(json!({}),&[])},
        {"name":"get_project_context","description":"Read compact project context when starting or resuming work; repeat calls include a delta.","inputSchema":schema(json!({"project":project}),&[])},
        {"name":"get_dossier","description":"Read full project decisions, sessions, lineage, artifacts and handoffs.","inputSchema":schema(json!({"project":project}),&[])},
        {"name":"search_history","description":"Search all agents before answering questions about earlier work or file origins; tool output is not indexed by default.","inputSchema":schema(json!({"query":text,"project":project,"agent":text,"kind":text,"limit":number}),&["query"])},
        {"name":"read_session","description":"Read a session transcript; long sessions return the latest messages, with paging and pre-compaction options.","inputSchema":schema(json!({"session_id":text,"max_messages":number,"line":number,"before_compact":boolean}),&["session_id"])},
        {"name":"read_native_memory","description":"Read backed-up native memory files or a specific revision.","inputSchema":schema(json!({"agent":text,"path":text,"file_id":number,"revision":number}),&[])},
        {"name":"create_handoff","description":"Save a project handoff, optionally linked to a durable task.","inputSchema":schema(json!({"project":project,"title":text,"done":text,"state":text,"decisions":text,"files_changed":text,"open_issues":text,"next_steps":text,"session_id":text,"task_id":text}),&[])},
        {"name":"get_recent_work","description":"Read recent sessions, latest handoff and active tasks.","inputSchema":schema(json!({"project":project,"limit":number}),&[])},
        {"name":"save_memory","description":"Save reusable knowledge with provenance. Decisions/rules need user confirmation; supersede duplicates and do not duplicate project files.","inputSchema":schema(json!({"type":text,"content":text,"project":project,"scope":text,"status":text,"session_id":text,"message_id":number}),&["content"])},
        {"name":"search_memory","description":"Search active curated memories unless another status is selected.","inputSchema":schema(json!({"query":text,"project":project,"scope":text,"type":text,"status":text,"agent":text,"limit":number}),&["query"])},
        {"name":"list_memories","description":"List curated memories with filters.","inputSchema":schema(json!({"project":project,"scope":text,"type":text,"status":text,"agent":text,"limit":number}),&[])},
        {"name":"update_memory","description":"Confirm, archive or supersede a memory; expected_updated_at rejects stale writes.","inputSchema":schema(json!({"id":text,"action":text,"superseded_by":text,"expected_updated_at":text}),&["id","action"])},
        {"name":"list_artifacts","description":"List file artifacts from agent sessions.","inputSchema":schema(json!({"project":project,"session_id":text,"limit":number}),&[])},
        {"name":"list_tasks","description":"List durable project tasks, not Agent todos.","inputSchema":schema(json!({"project":project,"status":text,"agent":text,"limit":number}),&[])},
        {"name":"create_task","description":"Create a project task; similar tasks require allow_duplicates=true to override.","inputSchema":schema(json!({"project":project,"title":text,"session_id":text,"allow_duplicates":boolean}),&["title"])},
        {"name":"get_task_context","description":"Read task evidence without linking. Pass a close_work token for native claim evidence.","inputSchema":schema(json!({"project":project,"task_id":text,"claim_token":text,"limit":number}),&["task_id"])},
        {"name":"update_task","description":"Update title/status with expected_revision; Agent conflicts with user-locked state become proposals.","inputSchema":schema(json!({"project":project,"task_id":text,"title":text,"status":text,"expected_revision":number,"session_id":text}),&["task_id"])},
        {"name":"link_task","description":"Link an imported session, or issue a one-time claim token when session_id is omitted.","inputSchema":schema(json!({"project":project,"task_id":text,"session_id":text,"relation":text}),&["task_id"])},
        {"name":"unlink_task","description":"Reject a task/session relation and suppress identical automatic evidence.","inputSchema":schema(json!({"project":project,"task_id":text,"session_id":text}),&["task_id","session_id"])},
        {"name":"close_work","description":"Save a handoff and issue a claim token; immediately pass it to get_task_context. Completion is not verification.","inputSchema":schema(json!({"project":project,"task_id":text,"title":text,"done":text,"state":text,"decisions":text,"files_changed":text,"open_issues":text,"next_steps":text,"status":text,"session_id":text}),&["task_id"])},
        {"name":"add_task_verification","description":"Record test results only from real tool_result evidence in a linked session; MCP cannot create manual verification.","inputSchema":schema(json!({"project":project,"task_id":text,"kind":text,"result":text,"source_message_id":number,"evidence":text}),&["task_id","result"])},
        {"name":"get_timeline","description":"Read the unified event timeline with time quality and keyset paging.","inputSchema":schema(json!({"project":project,"task_id":text,"from":text,"to":text,"kinds":{"type":"array","items":text},"cursor":text,"limit":number}),&[])},
        {"name":"get_changes","description":"Read generation-scoped change_log deltas; reset_required requires a full snapshot.","inputSchema":schema(json!({"cursor":{"type":"object","properties":{"database_generation":text,"seq":number}},"limit":number}),&[])},
        {"name":"list_project_records","description":"List file and SQLite project records with freshness and pin state.","inputSchema":schema(json!({"project":project}),&[])},
        {"name":"get_project_record","description":"Read a handoff (handoff:N) or document revision (file:file_id:revision).","inputSchema":schema(json!({"project":project,"record_id":text}),&["record_id"])},
        {"name":"search_work","description":"Search durable tasks, artifacts, handoffs and memories with provenance.","inputSchema":schema(json!({"query":text,"project":project,"limit":number}),&["query"])}
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_status_records_tool_outcomes_without_arguments() {
        let home = tempfile::tempdir().unwrap();
        let good = call_tool(
            home.path(),
            json!(1),
            &json!({"name":"search_history", "arguments":{"query":"private-search-text"}}),
        );
        assert_ne!(good["result"]["isError"], true);
        let read = || {
            crate::recall_status::read(home.path())
                .into_iter()
                .find(|r| r["name"] == "search_history")
                .unwrap()
        };
        assert_eq!(read()["ok"], true);
        assert_eq!(read()["result_count"], 0);
        let bad = call_tool(
            home.path(),
            json!(2),
            &json!({"name":"search_history", "arguments":{}}),
        );
        assert_eq!(bad["result"]["isError"], true);
        assert_eq!(read()["ok"], false);
        let raw =
            std::fs::read_to_string(home.path().join("recall-status/mcp-search_history.json"))
                .unwrap();
        assert!(!raw.contains("private-search-text"));
        assert!(!raw.contains("missing required argument"));
    }

    #[test]
    fn initialize_and_list() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".last_import"), []).unwrap(); // 新鲜单飞标记：init_import 跳过真实导入
        let init = handle(
            home.path(),
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "yourmem");

        let list = handle(
            home.path(),
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 26);

        // notifications get no response
        assert!(handle(
            home.path(),
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .is_none());
    }
}
