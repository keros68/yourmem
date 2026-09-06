//! MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Kept deliberately small: initialize, ping, tools/list, tools/call.
//! Agents launch `yourmem mcp`; the server's cwd is inherited from the
//! agent process, which is how project auto-detection works.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::db::{self, HandoffFields, SearchOpts};

const PROTOCOL_VERSION: &str = "2024-11-05";

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
                write_msg(&mut out, &error(Value::Null, -32700, &format!("parse error: {e}")))?;
                continue;
            }
        };
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
        return if has_id { Some(error(id, -32600, "invalid request: missing method")) } else { None };
    };

    // 消费入口自带新鲜度：回复 initialize 前先尽力增量导入（失败不传导）。
    if method == "initialize" {
        init_import(home);
    }
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

/// initialize 前同步增量导入（消费入口自带新鲜度）：尽力而为，失败只记 stderr 不传导；`.last_import` 60 秒单飞（watch/CLI/app 同钟）。
fn init_import(home: &Path) {
    let fresh = std::fs::metadata(home.join(".last_import")).and_then(|m| m.modified())
        .is_ok_and(|t| match t.elapsed() {
            Ok(d) => d < std::time::Duration::from_secs(60),
            // 未来 mtime（时钟回拨/NTP 校正）：按不新鲜处理，宁可多导一次
            Err(_) => false,
        });
    if fresh { return; }
    if let Err(e) = crate::ingest::import_defaults(home)
        .and_then(|_| db::log_usage(&db::open(home)?, "mcp", "import_on_init"))
    {
        eprintln!("yourmem mcp: import_on_init failed (continuing anyway): {e:#}");
    }
}

fn call_tool(home: &Path, id: Value, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let outcome = run_tool(home, name, &args);
    let count = outcome.as_ref().ok().and_then(|v| v["results"].as_array().or_else(|| v["messages"].as_array())).map(|v| v.len() as u64);
    crate::recall_status::record(home, "mcp", name, outcome.is_ok(), count);
    match outcome {
        Ok(v) => result(id, ok_text(v)),
        Err(e) => result(id, err_text(&format!("{e:#}"))),
    }
}

fn run_tool(home: &Path, name: &str, args: &Value) -> anyhow::Result<Value> {
    let conn = db::open(home)?;
    let _ = db::log_usage(&conn, "mcp", name); // 本地指标，失败静默
    let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
    let arg_str = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);

    match name {
        "list_projects" => Ok(json!({ "projects": db::list_projects(&conn)? })),

        "get_project_context" => {
            let (pid, ..) = db::resolve_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("no matching project; run `yourmem import` first"))?;
            let mut context = db::project_context(&conn, pid)?;
            context["source_review"] = crate::project_review::status(&conn, home, pid)?;
            Ok(context)
        }

        "get_dossier" => {
            let (pid, ..) = db::resolve_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("no matching project; run `yourmem import` first"))?;
            crate::dossier::project_dossier(&conn, pid)
        }

        "search_history" => {
            let query = arg_str("query")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
            let hits = db::search(&conn, &SearchOpts {
                query,
                project: arg_str("project"),
                agent: arg_str("agent"),
                kind: arg_str("kind"),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32,
            })?;
            Ok(json!({ "results": hits }))
        }

        "read_session" => {
            let sid = arg_str("session_id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: session_id"))?;
            let max = args.get("max_messages").and_then(Value::as_u64).unwrap_or(60) as u32;
            let line = args.get("line").and_then(Value::as_i64);
            let before_compact =
                args.get("before_compact").and_then(Value::as_bool).unwrap_or(false);
            db::read_session(&conn, &sid, max, line, before_compact)
        }

        "read_native_memory" => {
            let agent = arg_str("agent")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: agent"))?;
            crate::memfiles::read_native(&conn, home, &agent, arg_str("path").as_deref())
        }

        "create_handoff" => {
            let (pid, ..) = db::resolve_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("no matching project; run `yourmem import` first"))?;
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
            let id = db::create_handoff(&conn, pid, &fields)?;
            // 按 pid 直查：再走一次 resolve_project 会 fallback 到"最近活跃项目"，
            // handoff 目标不是它时把名字/路径丢成空串
            let (name, path) = conn
                .query_row(
                    "SELECT name, path FROM projects WHERE id = ?1",
                    rusqlite::params![pid],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .unwrap_or_default();
            Ok(json!({ "handoff_id": id, "project": name, "project_path": path }))
        }

        "get_recent_work" => {
            let pid = match arg_str("project") {
                Some(p) => Some(
                    db::resolve_project(&conn, Some(&p), cwd.as_deref())?
                        .ok_or_else(|| anyhow::anyhow!("no matching project"))?
                        .0,
                ),
                None => None,
            };
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32;
            let sessions = db::recent_sessions(&conn, pid, limit)?;
            let handoff = match pid {
                Some(pid) => db::latest_handoff(&conn, pid)?,
                None => None,
            };
            let tasks = db::open_tasks(&conn, pid)?;
            Ok(json!({ "recent_sessions": sessions, "latest_handoff": handoff, "open_tasks": tasks }))
        }

        "save_memory" => {
            let content = arg_str("content")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: content"))?;
            let r#type = arg_str("type").unwrap_or_else(|| "fact".to_string());
            let scope = arg_str("scope").unwrap_or_else(|| "project".to_string());
            let pid = if scope == "global" {
                None
            } else {
                Some(
                    db::resolve_project(&conn, arg_str("project").as_deref(), cwd.as_deref())?
                        .ok_or_else(|| anyhow::anyhow!("no matching project"))?
                        .0,
                )
            };
            let source_session = arg_str("session_id");
            // 写路径纪律（engramory 吸收）：查重提示随结果返回，agent 据 此
            // 判断"更新优于复制"（知情门控，不拦截）
            let (id, similar) = db::save_memory_with_similar(&conn, &db::MemoryInput {
                project_id: pid,
                scope: &scope,
                r#type: &r#type,
                content: &content,
                status: arg_str("status").as_deref(),
                source_session_id: source_session.as_deref(),
                source_message_id: args.get("message_id").and_then(Value::as_i64),
            })?;
            Ok(json!({
                "memory_id": id,
                "similar": similar,
                "note": if similar.is_empty() { Value::Null } else {
                    json!("检测到疑似重复：更新优于复制——若语义重复，用 update_memory supersede 旧条或请用户确认后再保留两条")
                },
            }))
        }

        "search_memory" => {
            let query = arg_str("query")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
            let pid = resolve_optional_project(&conn, &arg_str("project"), cwd.as_deref())?;
            let mems = db::search_memory(&conn, &query, &db::MemoryFilter {
                project_id: pid,
                scope: arg_str("scope"),
                r#type: arg_str("type"),
                status: arg_str("status"),
                agent: arg_str("agent"),
                include_global: pid.is_some(),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32,
            })?;
            Ok(json!({ "memories": mems }))
        }

        "list_memories" => {
            let pid = resolve_optional_project(&conn, &arg_str("project"), cwd.as_deref())?;
            let mems = db::list_memories(&conn, &db::MemoryFilter {
                project_id: pid,
                scope: arg_str("scope"),
                r#type: arg_str("type"),
                status: arg_str("status"),
                agent: arg_str("agent"),
                include_global: pid.is_some(),
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(50) as u32,
            })?;
            Ok(json!({ "memories": mems }))
        }

        "update_memory" => {
            let id = arg_str("id")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
            let action = arg_str("action")
                .ok_or_else(|| anyhow::anyhow!("missing required argument: action (confirm|archive|supersede)"))?;
            let by = arg_str("superseded_by");
            db::update_memory_status(&conn, &id, &action, by.as_deref())?;
            Ok(json!({ "memory_id": id, "action": action }))
        }

        "list_artifacts" => {
            let pid = resolve_optional_project(&conn, &arg_str("project"), cwd.as_deref())?;
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

fn resolve_optional_project(
    conn: &rusqlite::Connection,
    ident: &Option<String>,
    cwd: Option<&str>,
) -> anyhow::Result<Option<i64>> {
    match ident {
        Some(p) => Ok(Some(
            db::resolve_project(conn, Some(p), cwd)?
                .ok_or_else(|| anyhow::anyhow!("no matching project"))?
                .0,
        )),
        None => Ok(None),
    }
}

fn tools_list() -> Value {
    let project_prop = json!({ "type": "string", "description": "Project name or path fragment. Optional: defaults to the project matching the current working directory." });
    json!({
        "tools": [
            {
                "name": "list_projects",
                "description": "List all known projects with session/message counts and last activity.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "get_project_context",
                "description": "Compact project recall: confirmed memories (decisions/rules/facts), per-agent session counts, recent sessions and the latest handoff. Call this when starting or resuming work in a project instead of asking the user to re-explain context.",
                "inputSchema": { "type": "object", "properties": { "project": project_prop } }
            },
            {
                "name": "get_dossier",
                "description": "Full project dossier: decision board with superseded history (how decisions were overturned), session timeline with lineage marks, all artifacts, handoff chain. Deeper than get_project_context; use when reviewing a project's evolution.",
                "inputSchema": { "type": "object", "properties": { "project": project_prop } }
            },
            {
                "name": "search_history",
                "description": "USE THIS FIRST whenever the user asks whether something was done before, how it was done, or where a result/file came from ('之前是不是做过', '以前怎么处理过'). Searches history across ALL agents — tool output is not indexed by default — never conclude 'not done' from the repo alone. Returns session ids for read_session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search text (Chinese or English)." },
                        "project": project_prop,
                        "agent": { "type": "string", "description": "Filter by agent: claude | codex | opencode | zcode | kimi" },
                        "kind": { "type": "string", "description": "Filter by message kind: user | assistant | thinking | tool_call | tool_result | system | summary" },
                        "limit": { "type": "number", "description": "Max results (default 10)." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "read_session",
                "description": "Read one session's normalized transcript. Longer sessions return the LAST messages (current state); total_messages = full length. before_compact=true: only messages before the first context compaction (pre-compact backup).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string" },
                        "max_messages": { "type": "number" },
                        "line": { "type": "number", "description": "Focus the excerpt on this line_no (source-pointer jump)." },
                        "before_compact": { "type": "boolean", "description": "Pre-compaction portion only; no-op if never compacted." }
                    },
                    "required": ["session_id"]
                }
            },
            {
                "name": "read_native_memory",
                "description": "Read your own native memory files (MEMORY.md / AGENTS.md) as backed up by yourmem, with revision counts. Use when the user asks how a memory file evolved, or when you suspect your memory/instructions changed and want the previous content.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent": { "type": "string", "description": "claude | codex" },
                        "path": { "type": "string", "description": "Optional: exact path or suffix (e.g. 'MEMORY.md') to narrow to one file." }
                    },
                    "required": ["agent"]
                }
            },
            {
                "name": "create_handoff",
                "description": "Write a handoff for the current project at the end of a work session, so the next session — with any agent — can continue without re-explanation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": project_prop,
                        "title": { "type": "string" },
                        "done": { "type": "string", "description": "What was completed." },
                        "state": { "type": "string", "description": "Current state of the work." },
                        "decisions": { "type": "string", "description": "Important decisions made." },
                        "files_changed": { "type": "string" },
                        "open_issues": { "type": "string" },
                        "next_steps": { "type": "string" },
                        "session_id": { "type": "string", "description": "Provenance: the session this handoff summarizes." }
                    }
                }
            },
            {
                "name": "get_recent_work",
                "description": "What happened lately: recent sessions (all agents or one project) plus the latest handoff and open tasks.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": project_prop,
                        "limit": { "type": "number", "description": "Default 10." }
                    }
                }
            },
            {
                "name": "save_memory",
                "description": "Save reusable knowledge. Decisions/rules default to suggested until user confirmation. Pass session_id and message_id for provenance. Check similar: supersede duplicates instead of accumulating copies. For lessons/preferences include scope, failed attempts, Why, How-to-apply, actual validation results and review conditions; mark untested claims. Do not duplicate git, code or AGENTS.md/CLAUDE.md.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "description": "fact | decision | rule | task | lesson | preference | context (default fact)" },
                        "content": { "type": "string" },
                        "project": project_prop,
                        "scope": { "type": "string", "description": "global | project | session (default project)" },
                        "status": { "type": "string", "description": "suggested | confirmed (rarely needed; defaults are deliberate)" },
                        "session_id": { "type": "string", "description": "Provenance: session this memory comes from." },
                        "message_id": { "type": "number", "description": "Provenance: message id within the session." }
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "search_memory",
                "description": "Search curated memories (decisions, rules, tasks, facts…) across projects. By default only active memories (suggested + confirmed) are returned.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "project": project_prop,
                        "scope": { "type": "string" },
                        "type": { "type": "string" },
                        "status": { "type": "string" },
                        "agent": { "type": "string", "description": "Filter by source session's agent: claude | codex | opencode | zcode | kimi" },
                        "limit": { "type": "number", "description": "Default 10." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "list_memories",
                "description": "List memories with optional filters. By default returns active (suggested + confirmed) memories.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": project_prop,
                        "scope": { "type": "string" },
                        "type": { "type": "string" },
                        "status": { "type": "string" },
                        "agent": { "type": "string", "description": "Filter by source session's agent: claude | codex | opencode | zcode | kimi" },
                        "limit": { "type": "number", "description": "Default 50." }
                    }
                }
            },
            {
                "name": "update_memory",
                "description": "Change a memory's lifecycle state: confirm a suggestion, archive, or supersede it with a newer memory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "action": { "type": "string", "description": "confirm | archive | supersede" },
                        "superseded_by": { "type": "string", "description": "Required for supersede: id of the replacing memory." }
                    },
                    "required": ["id", "action"]
                }
            },
            {
                "name": "list_artifacts",
                "description": "List files produced by sessions (code, reports, notebooks…), e.g. 'that feature importance figure from last week'.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": project_prop,
                        "session_id": { "type": "string" },
                        "limit": { "type": "number", "description": "Default 50." }
                    }
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_status_records_tool_outcomes_without_arguments() {
        let home = tempfile::tempdir().unwrap();
        let good = call_tool(home.path(), json!(1), &json!({"name":"search_history", "arguments":{"query":"private-search-text"}}));
        assert_ne!(good["result"]["isError"], true);
        let read = || crate::recall_status::read(home.path()).into_iter().find(|r| r["name"]=="search_history").unwrap();
        assert_eq!(read()["ok"], true);
        assert_eq!(read()["result_count"], 0);
        let bad = call_tool(home.path(), json!(2), &json!({"name":"search_history", "arguments":{}}));
        assert_eq!(bad["result"]["isError"], true);
        assert_eq!(read()["ok"], false);
        let raw = std::fs::read_to_string(home.path().join("recall-status/mcp-search_history.json")).unwrap();
        assert!(!raw.contains("private-search-text"));
        assert!(!raw.contains("missing required argument"));
    }

    #[test]
    fn initialize_and_list() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".last_import"), []).unwrap(); // 新鲜单飞标记：init_import 跳过真实导入
        let init = handle(home.path(), &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "yourmem");

        let list = handle(home.path(), &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 13);

        // notifications get no response
        assert!(handle(home.path(), &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
    }
}
