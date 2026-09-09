//! 回忆层（DESIGN-0.3 §7）：确定性聚合视图，零新采集、零 LLM。
//! 卷宗/日报里的每一条都带来源指针（会话+消息行号 / memory id），阅读视图永远
//! 不是事实来源——vault 才是。导出即 Wiki：markdown 是导出物，可进 git。
//!
//! 评审加固（codex 2026-08-23，9 blocker）：聚合包一层只读事务取一致快照；
//! 决策板只展示 confirmed（§7.2），superseded 折叠为演变史；演变链带环检测
//! （数据层 confirm 已清 superseded_by，这里仍防御——老库可能已有环）；
//! 日报按本地日的 UTC 区间匹配（时间戳存 UTC Z，东八区早晚边界会错位一天）。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

mod tasks;

/// 项目卷宗（§7.2）：概览条 / 决策板（confirmed 活跃，superseded 演变史）/
/// 时间线 / 任务状态时间线（§7.7，读时派生）/ artifacts 画廊（全量，§7.2 明确要求）/
/// handoff 链。
/// 只读事务包住全部查询：并发导入时不会出现"概览旧快照、时间线新快照"的自相矛盾。
pub fn project_dossier(conn: &Connection, project_id: i64) -> Result<Value> {
    let tx = conn.unchecked_transaction()?;
    let (name, path): (String, String) =
        tx.query_row("SELECT name, path FROM projects WHERE id = ?1", params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;

    // 概览条（无 token 统计——库里没有，用消息数代替）
    let overview = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(message_count),0), COUNT(DISTINCT agent),
                MIN(started_at), MAX(ended_at)
         FROM sessions WHERE project_id = ?1 AND deleted_at IS NULL",
        params![project_id],
        |r| {
            Ok(json!({
                "sessions": r.get::<_, i64>(0)?,
                "messages": r.get::<_, i64>(1)?,
                "agents": r.get::<_, i64>(2)?,
                "first_activity": r.get::<_, Option<String>>(3)?,
                "last_activity": r.get::<_, Option<String>>(4)?,
            }))
        },
    )?;
    let mem_counts = {
        let mut stmt = tx.prepare(
            "SELECT status, COUNT(*) FROM memories WHERE project_id = ?1 OR project_id IS NULL GROUP BY status",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| Ok(json!({ "status": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? })))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    // 决策板数据（§7.2：confirmed 活跃；superseded 折叠为演变史——数据契约层就
    // 排除 suggested/archived，别让每个渲染端各自过滤）。行号 JOIN 必须同校验
    // 会话归属：只按 message id 关联会把别的会话的行号错挂过来（伪造指针）。
    let decisions = {
        let mut stmt = tx.prepare(
            "SELECT m.id, m.type, m.content, m.status, m.superseded_by, m.source_session_id,
                    m.source_message_id, msg.line_no, m.created_at, m.updated_at
             FROM memories m
             LEFT JOIN messages msg ON msg.id = m.source_message_id
                  AND msg.session_id = m.source_session_id
             WHERE (m.project_id = ?1 OR m.project_id IS NULL) AND m.type IN ('decision','rule')
               AND m.status IN ('confirmed','superseded')
             ORDER BY m.created_at, m.id",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "type": r.get::<_, String>(1)?,
                    "content": r.get::<_, String>(2)?,
                    "status": r.get::<_, String>(3)?,
                    "superseded_by": r.get::<_, Option<String>>(4)?,
                    "source_session_id": r.get::<_, Option<String>>(5)?,
                    "source_message_id": r.get::<_, Option<i64>>(6)?,
                    "source_line_no": r.get::<_, Option<i64>>(7)?,
                    "created_at": r.get::<_, String>(8)?,
                    "updated_at": r.get::<_, String>(9)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    // 时间线：会话按日排列，fork/compact/continuation 打标记（来源指针 = session_id）
    let timeline = {
        let mut stmt = tx.prepare(
            "SELECT s.id, s.agent, s.started_at, s.ended_at, s.message_count,
                    (SELECT l.link_type || '|' || l.parent_session_id FROM session_links l
                     WHERE l.child_session_id = s.id LIMIT 1),
                    (SELECT COUNT(*) FROM session_artifacts a WHERE a.session_id = s.id),
                    (SELECT replace(substr(m.content, 1, 80), char(10), ' ') FROM messages m
                     WHERE m.session_id = s.id AND m.kind = 'user'
                       AND substr(ltrim(m.content), 1, 1) NOT IN ('<', '#')
                     ORDER BY m.line_no, m.ord LIMIT 1)
             FROM sessions s
             WHERE s.project_id = ?1 AND s.deleted_at IS NULL
             ORDER BY s.started_at, s.id",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| {
                // link_type 与 parent 必须来自同一条 link：两个独立 LIMIT 1 子查询
                // 在一 child 多链接时可能各取各行，拼出假父子
                let link = r.get::<_, Option<String>>(5)?
                    .map(|s| match s.split_once('|') {
                        Some((t, p)) => (t.to_string(), p.to_string()),
                        None => (s, String::new()),
                    });
                Ok(json!({
                    "session_id": r.get::<_, String>(0)?,
                    "agent": r.get::<_, String>(1)?,
                    "started_at": r.get::<_, Option<String>>(2)?,
                    "ended_at": r.get::<_, Option<String>>(3)?,
                    "messages": r.get::<_, i64>(4)?,
                    "link_type": link.as_ref().map(|(t, _)| t.clone()),
                    "parent_session_id": link.map(|(_, p)| p),
                    "artifacts": r.get::<_, i64>(6)?,
                    "title": r.get::<_, Option<String>>(7)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let lineage_edges = {
        let mut stmt = tx.prepare("SELECT l.parent_session_id, l.child_session_id, l.link_type
            FROM session_links l JOIN sessions s ON s.id=l.child_session_id
            WHERE s.project_id=?1 AND s.deleted_at IS NULL
            ORDER BY s.started_at, s.id, l.parent_session_id, l.link_type")?;
        let rows = stmt.query_map([project_id], |r| Ok(json!({
            "p": r.get::<_, String>(0)?, "c": r.get::<_, String>(1)?, "lt": r.get::<_, String>(2)?
        })))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    // Handoff 链：交接历史一览
    let handoffs = {
        let mut stmt = tx.prepare(
            "SELECT id, session_id, title, next_steps, created_at FROM handoffs
             WHERE project_id = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "session_id": r.get::<_, Option<String>>(1)?,
                    "title": r.get::<_, String>(2)?,
                    "next_steps": r.get::<_, String>(3)?,
                    "created_at": r.get::<_, String>(4)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    // §7.2 要求"全部"写文件产物——不设 200 上限（u32::MAX 交给 SQLite LIMIT）
    let artifacts = crate::db::list_artifacts(&tx, Some(project_id), None, u32::MAX)?;

    // 任务状态时间线（§7.7，0.4.3）：读时派生 agent 自报任务状态——tool_call 已在
    // messages，按 agent 门控前缀匹配（tasks::MATCHER_SQL，codex 刻意不匹配），
    // 解析/折叠语义见 tasks 模块。零采集改动、零 schema 变更。
    let task_timeline = {
        let rows = {
            let mut stmt = tx.prepare(&format!(
                "SELECT m.session_id, s.agent, s.started_at, m.line_no, m.timestamp, m.content
                 FROM messages m
                 JOIN sessions s ON s.id = m.session_id
                 WHERE s.project_id = ?1 AND s.deleted_at IS NULL AND m.kind = 'tool_call'
                   AND ({})
                 ORDER BY s.started_at, m.session_id, m.line_no, m.ord",
                tasks::MATCHER_SQL
            ))?;
            let rows = stmt
                .query_map(params![project_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        // 会话分组保持 SQL 的 started_at 顺序；解析失败的事件静默跳过（防御式）
        let mut order: Vec<String> = Vec::new();
        let mut per_session: HashMap<String, (String, Vec<(i64, Option<String>, tasks::TaskEvent)>)> =
            HashMap::new();
        for (sid, agent, _started, line_no, ts, content) in rows {
            let Some(ev) = tasks::parse_event(&agent, &content) else {
                continue;
            };
            per_session
                .entry(sid.clone())
                .or_insert_with(|| {
                    order.push(sid.clone());
                    (agent, Vec::new())
                })
                .1
                .push((line_no, ts, ev));
        }
        let task_json = |t: &tasks::TaskItem| {
            json!({ "id": t.id, "content": t.content, "status": t.status })
        };
        order
            .into_iter()
            .filter_map(|sid| {
                let (agent, events) = per_session.remove(&sid)?;
                let changes = tasks::fold(events);
                let fin = changes.last()?;
                let todos_json: Vec<Value> =
                    fin.todos.iter().map(&task_json).collect();
                let total = fin.todos.len();
                let done = fin.todos.iter().filter(|t| t.status == "completed").count();
                Some(json!({
                    "session_id": sid,
                    "agent": agent,
                    "changes": changes.iter().map(|c| json!({
                        "line_no": c.line_no,
                        "timestamp": c.timestamp,
                        "todos": c.todos.iter().map(&task_json).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                    "final": {
                        "line_no": fin.line_no,
                        "timestamp": fin.timestamp,
                        "todos": todos_json,
                    },
                    "done": done,
                    "total": total,
                }))
            })
            .collect::<Vec<_>>()
    };

    tx.commit()?;
    Ok(json!({
        "project": name,
        "path": path,
        "overview": overview,
        "memory_counts": mem_counts,
        "decisions": decisions,
        "timeline": timeline,
        "lineage_edges": lineage_edges,
        "task_timeline": task_timeline,
        "artifacts": artifacts,
        "handoffs": handoffs,
    }))
}

/// 日报卡（§7.3）：当日会话/项目分布/新增决策/open tasks 遗留/最近 handoff。
/// 时间戳存 UTC（RFC3339 Z），"当天"按本地时区换算成 UTC 区间再匹配——
/// 直接拿本地日期前缀匹配 UTC 时间戳，东八区 00:00–08:00 的会话会漏一天。
pub fn daily_digest(conn: &Connection, day: &str) -> Result<Value> {
    let (lo, hi) = day_bounds_utc(day)?;
    let tx = conn.unchecked_transaction()?;
    let in_day = "(started_at >= ?1 AND started_at < ?2) OR (ended_at >= ?1 AND ended_at < ?2)";
    // messages 口径：跨日会话的全部历史消息计入其落入当日的每一天（计数语义
    // 是"活跃度"而非"当日新消息"——SUM 比 message 表按时间戳数便宜且够用）
    let (sessions, messages): (i64, i64) = tx.query_row(
        &format!("SELECT COUNT(*), COALESCE(SUM(message_count),0) FROM sessions WHERE deleted_at IS NULL AND ({in_day})"),
        params![lo, hi],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let projects = {
        let mut stmt = tx.prepare(
            &format!(
                "SELECT p.id, p.name, p.path, COUNT(*), COALESCE(SUM(s.message_count),0)
                 FROM sessions s JOIN projects p ON p.id = s.project_id
                 WHERE s.deleted_at IS NULL AND ({in_day})
                 GROUP BY p.id ORDER BY COUNT(*) DESC"
            ),
        )?;
        let rows = stmt
            .query_map(params![lo, hi], |r| {
                Ok(json!({
                    "project_id": r.get::<_, i64>(0)?,
                    "project": r.get::<_, String>(1)?,
                    "path": r.get::<_, String>(2)?,
                    "sessions": r.get::<_, i64>(3)?,
                    "messages": r.get::<_, i64>(4)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    let count = |sql: &str| -> Result<i64> {
        Ok(tx.query_row(sql, params![lo, hi], |r| r.get(0))?)
    };
    let memories_added = count("SELECT COUNT(*) FROM memories WHERE created_at >= ?1 AND created_at < ?2")?;
    let decisions_added = count(
        "SELECT COUNT(*) FROM memories WHERE created_at >= ?1 AND created_at < ?2 AND type IN ('decision','rule')")?;
    let artifacts_added = count(
        "SELECT COUNT(*) FROM session_artifacts a JOIN sessions s ON s.id = a.session_id
         WHERE s.deleted_at IS NULL AND a.created_at >= ?1 AND a.created_at < ?2",
    )?;
    let open_tasks = crate::db::open_tasks(&tx, None)?;
    // 工作账本：会话是保存单位，项目才是用户理解每天工作的单位。这里只做
    // 确定性归并，标题/末条回复/产物/待办都保留来源，不生成新的事实。
    let mut project_activity = Vec::new();
    for p in &projects {
        let pid = p["project_id"].as_i64().unwrap_or(0);
        let activities = {
            let mut stmt = tx.prepare(&format!(
                "SELECT s.id, s.agent, s.started_at, s.ended_at, s.message_count,
                        (SELECT replace(substr(m.content,1,240),char(10),' ') FROM messages m
                         WHERE m.session_id=s.id AND m.kind='user'
                           AND substr(ltrim(m.content),1,1) NOT IN ('<','#')
                         ORDER BY m.line_no,m.ord LIMIT 1),
                        (SELECT substr(m.content,1,1600) FROM messages m
                         WHERE m.session_id=s.id AND m.kind='assistant'
                         ORDER BY m.line_no DESC,m.ord DESC LIMIT 1),
                        (SELECT COUNT(*) FROM session_artifacts a WHERE a.session_id=s.id)
                 FROM sessions s WHERE s.project_id=?3 AND s.deleted_at IS NULL AND ({in_day})
                 ORDER BY COALESCE(s.ended_at,s.started_at) DESC,s.id"
            ))?;
            let rows = stmt.query_map(params![lo, hi, pid], |r| Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "agent": r.get::<_, String>(1)?,
                "started_at": r.get::<_, Option<String>>(2)?,
                "ended_at": r.get::<_, Option<String>>(3)?,
                "messages": r.get::<_, i64>(4)?,
                "title": r.get::<_, Option<String>>(5)?,
                "tail": r.get::<_, Option<String>>(6)?,
                "artifact_count": r.get::<_, i64>(7)?,
            })))?.collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let agents = {
            let mut stmt = tx.prepare(&format!(
                "SELECT s.agent,COUNT(*),COALESCE(SUM(s.message_count),0)
                 FROM sessions s WHERE s.project_id=?3 AND s.deleted_at IS NULL AND ({in_day})
                 GROUP BY s.agent ORDER BY COUNT(*) DESC,s.agent"
            ))?;
            let rows = stmt.query_map(params![lo, hi, pid], |r| Ok(json!({
                "agent": r.get::<_, String>(0)?,
                "sessions": r.get::<_, i64>(1)?,
                "messages": r.get::<_, i64>(2)?,
            })))?.collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let artifacts = {
            let mut stmt = tx.prepare(
                "SELECT a.path,a.tool,a.session_id,s.agent,a.created_at
                 FROM session_artifacts a JOIN sessions s ON s.id=a.session_id
                 WHERE a.project_id=?3 AND s.deleted_at IS NULL
                   AND a.created_at>=?1 AND a.created_at<?2
                 ORDER BY a.created_at DESC,a.id DESC",
            )?;
            let rows = stmt.query_map(params![lo, hi, pid], |r| Ok(json!({
                "path": r.get::<_, String>(0)?,
                "tool": r.get::<_, Option<String>>(1)?,
                "session_id": r.get::<_, String>(2)?,
                "agent": r.get::<_, String>(3)?,
                "created_at": r.get::<_, String>(4)?,
            })))?.collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let project_name = p["project"].as_str().unwrap_or("");
        let tasks: Vec<Value> = open_tasks.iter()
            .filter(|t| t["project"].as_str() == Some(project_name))
            .cloned().collect();
        project_activity.push(json!({
            "project_id": pid,
            "project": project_name,
            "path": p["path"],
            "sessions": p["sessions"],
            "messages": p["messages"],
            "agents": agents,
            "activities": activities,
            "artifacts": artifacts,
            "open_tasks": tasks,
            "latest_handoff": crate::db::latest_handoff(&tx, pid)?,
        }));
    }
    let recent_handoffs = {
        let mut stmt = tx.prepare(
            "SELECT h.id, h.title, h.next_steps, h.created_at, p.name FROM handoffs h
             JOIN projects p ON p.id = h.project_id ORDER BY h.id DESC LIMIT 3",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "title": r.get::<_, String>(1)?,
                    "next_steps": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, String>(3)?,
                    "project": r.get::<_, String>(4)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    tx.commit()?;
    Ok(json!({
        "day": day,
        "sessions": sessions,
        "messages": messages,
        "projects": projects,
        "project_activity": project_activity,
        "memories_added": memories_added,
        "decisions_added": decisions_added,
        "artifacts_added": artifacts_added,
        "open_tasks": open_tasks,
        "recent_handoffs": recent_handoffs,
    }))
}

/// 本地日的 UTC 边界（闭开区间）。两个午夜各自换算——DST 切换日不是 24 小时整，
/// "单一偏移 +86400" 的近似在切换日必错（codex 用 America/New_York 复现）。
/// tz 参数化是为了用 FixedOffset 写确定性单测，不依赖进程时区。
fn bounds_in_tz<Tz: chrono::TimeZone>(day: &str, tz: &Tz) -> Result<(String, String)> {
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("day 应为 YYYY-MM-DD: {e}"))?;
    let next = d.succ_opt().ok_or_else(|| anyhow::anyhow!("invalid day"))?;
    let midnight_utc = |date: chrono::NaiveDate| -> Result<String> {
        let ndt = date.and_hms_opt(0, 0, 0).ok_or_else(|| anyhow::anyhow!("invalid day"))?;
        // 个别时区（如 Havana）在午夜回拨，本地午夜出现两次——取第一次，
        // 边界差一小时远好过整天报错（codex 用 America/Havana 复现过失败）。
        let local = tz
            .from_local_datetime(&ndt)
            .earliest()
            .ok_or_else(|| anyhow::anyhow!("本地午夜无法换算"))?;
        Ok(local.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    };
    Ok((midnight_utc(d)?, midnight_utc(next)?))
}

pub fn day_bounds_utc(day: &str) -> Result<(String, String)> {
    bounds_in_tz(day, &chrono::Local)
}

/// 导出即 Wiki（§7.1）：确定性 markdown，含全部来源指针，可进 git 可 diff。
/// 决策板/时间线附 Mermaid 图（GitHub/Obsidian 原生渲染）——图只承担"链的形状"，
/// 全文信息仍在列表里，两处独立可读。
pub fn render_markdown(d: &Value) -> String {
    let mut s = String::new();
    let o = &d["overview"];
    let _ = writeln!(s, "# 项目卷宗：{}", d["project"].as_str().unwrap_or(""));
    let _ = writeln!(s, "\n> {} ｜ 活跃 {} → {} ｜ 会话 {} ｜ 消息 {} ｜ agent {} 种\n",
        d["path"].as_str().unwrap_or(""),
        short(o["first_activity"].as_str()), short(o["last_activity"].as_str()),
        o["sessions"], o["messages"], o["agents"]);

    // 决策板（§7.2）：confirmed 带头，superseded 折叠为演变史；环防御——
    // 数据层 confirm 已清 superseded_by，但老库可能已有环，visited 兜底。
    let _ = writeln!(s, "## 决策板\n");
    let decs = d["decisions"].as_array().cloned().unwrap_or_default();
    let mut prev_of: HashMap<String, Vec<&Value>> = HashMap::new();
    // 多条旧记忆可以合并到同一条新记忆，保留所有前驱。
    for m in &decs {
        if let Some(by) = m["superseded_by"].as_str() {
            prev_of.entry(by.to_string()).or_default().push(m);
        }
    }
    let source = |m: &Value| -> String {
        match (m["source_session_id"].as_str(), m["source_line_no"].as_i64()) {
            (Some(sid), Some(line)) => code(&format!("{sid}#L{line}")),
            (Some(sid), None) => code(sid),
            _ => "—".to_string(),
        }
    };
    let mut chained: HashSet<String> = HashSet::new();
    for head in decs.iter().filter(|m| m["status"] == "confirmed") {
        let _ = writeln!(s, "- [confirmed] **{}**（{}，来源 {}，id {}）",
            inline(head["content"].as_str().unwrap_or("")), short(head["created_at"].as_str()),
            source(head), code(head["id"].as_str().unwrap_or("")));
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(head["id"].as_str().unwrap_or("").to_string());
        let mut queue = std::collections::VecDeque::from(prev_of.get(head["id"].as_str().unwrap_or("")).cloned().unwrap_or_default());
        while let Some(prev) = queue.pop_front() {
            let pid = prev["id"].as_str().unwrap_or("");
            if !visited.insert(pid.to_string()) {
                continue; // 环或汇合节点重复出现时跳过，继续其他分支
            }
            chained.insert(pid.to_string());
            let _ = writeln!(s, "  - ↩ 演变史：[superseded] {}（{}，来源 {}，id {}）",
                inline(prev["content"].as_str().unwrap_or("")), short(prev["created_at"].as_str()),
                source(prev), code(pid));
            queue.extend(prev_of.get(pid).into_iter().flatten().copied());
        }
    }
    // 链外的 superseded（取代者不在本板，比如被 confirm 清链前的历史）兜底列出
    for m in decs.iter().filter(|m| m["status"] == "superseded" && !chained.contains(m["id"].as_str().unwrap_or(""))) {
        let _ = writeln!(s, "- **[superseded]** {}（{}，被 {} 取代，来源 {}，id {}）",
            inline(m["content"].as_str().unwrap_or("")), short(m["created_at"].as_str()),
            code(m["superseded_by"].as_str().unwrap_or("—")), source(m), code(m["id"].as_str().unwrap_or("")));
    }
    if decs.iter().all(|m| m["status"] != "confirmed" && m["status"] != "superseded") {
        let _ = writeln!(s, "（暂无已确认的决策/规则记忆）");
    }
    if let Some(g) = decision_chain_mermaid(&decs) {
        let _ = writeln!(s, "\n### 决策演变链\n\n{g}\n");
    }

    let _ = writeln!(s, "\n## 时间线\n");
    let tl = d["timeline"].as_array().cloned().unwrap_or_default();
    for t in &tl {
        let mark = match t["link_type"].as_str() {
            Some(lt) => format!("，{lt} ← {}", code(t["parent_session_id"].as_str().unwrap_or(""))),
            None => String::new(),
        };
        let arts = t["artifacts"].as_i64().unwrap_or(0);
        let _ = writeln!(s, "- {} `{}` {} 条消息{}{}｜ 来源 {}",
            t["started_at"].as_str().unwrap_or("—"), t["agent"].as_str().unwrap_or(""),
            t["messages"], mark,
            if arts > 0 { format!("，artifact {arts} 个") } else { String::new() },
            code(t["session_id"].as_str().unwrap_or("")));
    }
    if let Some(g) = lineage_mermaid(&tl, d["lineage_edges"].as_array()) {
        let _ = writeln!(s, "\n### 会话谱系\n\n{g}\n");
    }

    // 任务状态（§7.7，0.4.3）：每会话一条最终状态的 checkbox 列表（GFM 渲染），
    // 来源指到最后一个变化点；演变全史在 JSON 的 task_timeline.changes 里
    let _ = writeln!(s, "\n## 任务状态\n");
    let tasks = d["task_timeline"].as_array().cloned().unwrap_or_default();
    for t in &tasks {
        let fin = &t["final"];
        let changes = t["changes"].as_array().map(Vec::len).unwrap_or(0);
        let _ = writeln!(
            s,
            "- `{}` `{}` 完成 {}/{}（{changes} 次变化，来源 {}）",
            t["agent"].as_str().unwrap_or(""),
            t["session_id"].as_str().unwrap_or(""),
            t["done"],
            t["total"],
            code(&format!(
                "{}#L{}",
                t["session_id"].as_str().unwrap_or(""),
                fin["line_no"].as_i64().unwrap_or(0)
            ))
        );
        for td in fin["todos"].as_array().cloned().unwrap_or_default() {
            let status = td["status"].as_str().unwrap_or("");
            let mark = if status == "completed" { "x" } else { " " };
            let active = if status == "in_progress" { "（进行中）" } else { "" };
            let _ = writeln!(s, "  - [{mark}] {}{active}", inline(td["content"].as_str().unwrap_or("")));
        }
    }
    if tasks.is_empty() {
        let _ = writeln!(s, "（暂无）");
    }

    let _ = writeln!(s, "\n## Artifacts（全部 {} 项）\n", d["artifacts"].as_array().map(Vec::len).unwrap_or(0));
    let arts = d["artifacts"].as_array().cloned().unwrap_or_default();
    for a in &arts {
        let _ = writeln!(s, "- {}（{}，来源 {}）", code(a["path"].as_str().unwrap_or("")),
            a["tool"].as_str().unwrap_or("—"), code(a["session_id"].as_str().unwrap_or("")));
    }
    if arts.is_empty() {
        let _ = writeln!(s, "（暂无）");
    }

    let _ = writeln!(s, "\n## Handoff 链\n");
    let hos = d["handoffs"].as_array().cloned().unwrap_or_default();
    for h in &hos {
        let next = inline(h["next_steps"].as_str().unwrap_or(""));
        let _ = writeln!(s, "- {} **{}**{}（来源 {}）", h["created_at"].as_str().unwrap_or(""),
            inline(h["title"].as_str().unwrap_or("")),
            if next.is_empty() { String::new() } else { format!("：下一步 {next}") },
            code(h["session_id"].as_str().unwrap_or("—")));
    }
    if hos.is_empty() {
        let _ = writeln!(s, "（暂无）");
    }
    s
}

/// 动态日报 markdown 导出（§7.3"可导出 markdown"）。
pub fn render_digest_markdown(d: &Value) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# 动态：{}\n", d["day"].as_str().unwrap_or(""));
    let _ = writeln!(s, "- 会话 {} 个，消息 {} 条",
        d["sessions"].as_i64().unwrap_or(0), d["messages"].as_i64().unwrap_or(0));
    for p in d["projects"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(s, "  - {}：{} 会话 / {} 消息", p["project"].as_str().unwrap_or(""),
            p["sessions"], p["messages"]);
    }
    let _ = writeln!(s, "- 新增记忆 {} 条（其中决策/规则 {}）",
        d["memories_added"].as_i64().unwrap_or(0), d["decisions_added"].as_i64().unwrap_or(0));
    let _ = writeln!(s, "- 新增 artifact {} 个", d["artifacts_added"].as_i64().unwrap_or(0));
    let tasks = d["open_tasks"].as_array().cloned().unwrap_or_default();
    if !tasks.is_empty() {
        let _ = writeln!(s, "\n## 未完成任务遗留\n");
        for t in &tasks {
            let _ = writeln!(s, "- [ ] {}", inline(t["content"].as_str().unwrap_or("")));
        }
    }
    let hos = d["recent_handoffs"].as_array().cloned().unwrap_or_default();
    if !hos.is_empty() {
        let _ = writeln!(s, "\n## 最近 Handoff\n");
        for h in &hos {
            let _ = writeln!(s, "- **{}**（{}，{}）{}",
                inline(h["title"].as_str().unwrap_or("")), h["project"].as_str().unwrap_or(""),
                short(h["created_at"].as_str()),
                h["next_steps"].as_str().map(inline).filter(|n| !n.is_empty()).map(|n| format!("：下一步 {n}")).unwrap_or_default());
        }
    }
    s
}

/// 项目上下文一页导出（0.3.9，对位 Unabyss 的 context files 导出）：给不支持
/// MCP 的 AI（ChatGPT 网页版/Perplexity 等）直接粘贴用。紧凑优先——只放模型
/// 续聊真正需要的：概况、confirmed 记忆、handoff 下一步。
pub fn render_context_markdown(d: &Value) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# 项目上下文：{}", d["project"].as_str().unwrap_or(""));
    let _ = writeln!(s, "\n> 路径 {} ｜ artifact {} 个\n",
        d["path"].as_str().unwrap_or(""), d["artifact_count"]);
    let _ = writeln!(s, "## 各 agent 参与情况\n");
    for a in d["by_agent"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(s, "- `{}` {} 会话 / {} 消息，最近 {}",
            a["agent"].as_str().unwrap_or(""), a["sessions"], a["messages"],
            short(a["last_activity"].as_str()));
    }
    let _ = writeln!(s, "\n## 已确认记忆\n");
    let mems = d["memories"]["confirmed"].as_array().cloned().unwrap_or_default();
    for m in &mems {
        let _ = writeln!(s, "- **[{}]** {}（{}）",
            m["type"].as_str().unwrap_or("?"),
            inline(m["content"].as_str().unwrap_or("")),
            short(m["created_at"].as_str()));
    }
    if mems.is_empty() {
        let _ = writeln!(s, "（暂无）");
    }
    let pending = d["memories"]["suggested_pending"].as_i64().unwrap_or(0);
    if pending > 0 {
        let _ = writeln!(s, "\n（另有 {pending} 条建议记忆待确认）");
    }
    let h = &d["latest_handoff"];
    if h.is_object() {
        let _ = writeln!(s, "\n## 最近交接：{}\n", h["title"].as_str().unwrap_or(""));
        for (k, label) in [("done", "已完成"), ("state", "状态"), ("decisions", "决定"), ("files_changed", "改动文件"), ("next_steps", "下一步"), ("open_issues", "遗留问题")] {
            let v = inline(h[k].as_str().unwrap_or(""));
            if !v.is_empty() {
                let _ = writeln!(s, "- **{label}**：{v}");
            }
        }
    }
    s
}

/// 会话压缩前备份导出（0.3.8）：read_session(before_compact=true) 的 JSON 转可读
/// markdown。与卷宗不同，消息正文保持原样不内联化——这是"原文备份"，保真优先
/// 于版式（正文里的 markdown 就是当年的正文）。
pub fn render_session_markdown(d: &Value) -> String {
    let s = &d["session"];
    let mut out = String::new();
    let compact = match s["compact_line_no"].as_i64() {
        Some(l) => format!("行 {l}"),
        None => "无（会话未压缩，以下为全文）".to_string(),
    };
    let _ = writeln!(out, "# 压缩前对话备份：{}", s["native_id"].as_str().unwrap_or(""));
    let _ = writeln!(out, "\n> {} ｜ {} ｜ 压缩点 {} ｜ 备份消息 {} 条（会话共 {} 条）\n",
        s["agent"].as_str().unwrap_or(""),
        s["project"].as_str().unwrap_or("—"),
        compact,
        d["total_messages"],
        s["message_count"]);
    for m in d["messages"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(out, "## [{}] {}\n",
            m["kind"].as_str().unwrap_or("?"), short(m["timestamp"].as_str()));
        let _ = writeln!(out, "{}\n", m["content"].as_str().unwrap_or(""));
    }
    out
}

fn short(ts: Option<&str>) -> String {
    ts.map(|t| t.replace('T', " ").chars().take(16).collect()).unwrap_or_else(|| "—".into())
}

/// 内联化（S15）：memory/handoff 文本是自由文本，换行折叠为空格——否则列表项断裂、
/// 行首 "## " 会注入章节标题（recent_sessions 的 preview 子查询同手法）
fn inline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// code span 按内容里最长反引号串加围（S15）：内容含 ` 时单反引号围不住
fn code(s: &str) -> String {
    let longest = s.chars().fold((0usize, 0usize), |(max, cur), c| {
        if c == '`' { (max.max(cur + 1), cur + 1) } else { (max, 0) }
    }).0;
    let fence = "`".repeat(longest + 1);
    if longest > 0 { format!("{fence} {s} {fence}") } else { format!("{fence}{s}{fence}") }
}

/// Mermaid 节点标签清洗：折叠空白 → 去掉破坏 `["…"]` 语法/触发实体语法
/// （`#xxx;`）的字符 → 再折叠 → 截断。不追求保真——图只承担形状，全文在列表里；
/// 换行/引号/井号混进图代码会让整块渲染失败，宁可丢字不可破图。
fn mmd_label(s: &str, max_chars: usize) -> String {
    let cleaned: String = inline(s)
        .chars()
        .map(|c| match c {
            '"' | '\'' | '`' | '[' | ']' | '(' | ')' | '{' | '}' | '<' | '>'
            | '|' | '#' | ';' => ' ',
            _ => c,
        })
        .collect();
    let cleaned = inline(&cleaned);
    if cleaned.chars().count() > max_chars {
        format!("{}…", cleaned.chars().take(max_chars).collect::<String>())
    } else {
        cleaned
    }
}

/// 决策演变链 Mermaid（graph LR：旧在左，superseded 指向取代者）。
/// 只画链上节点——孤立决策列表已覆盖，图只画"链"（show-me 纪律：最小视图）。
/// 节点 id = 数组下标 m{i}，与输入顺序绑定，输出确定可 diff。
fn decision_chain_mermaid(decs: &[Value]) -> Option<String> {
    let id_of: HashMap<&str, String> = decs
        .iter()
        .enumerate()
        .map(|(i, m)| (m["id"].as_str().unwrap_or(""), format!("m{i}")))
        .collect();
    let mut edges: Vec<(String, String)> = Vec::new(); // (旧, 新)
    for m in decs {
        if let (Some(id), Some(by)) = (m["id"].as_str(), m["superseded_by"].as_str()) {
            if id != by && id_of.contains_key(by) {
                edges.push((id_of[id].clone(), id_of[by].clone()));
            }
        }
    }
    if edges.is_empty() {
        return None;
    }
    let mut g = String::from("```mermaid\ngraph LR\n");
    let mut old: Vec<String> = Vec::new();
    for (i, m) in decs.iter().enumerate() {
        let nid = format!("m{i}");
        if !edges.iter().any(|(a, b)| a == &nid || b == &nid) {
            continue;
        }
        let _ = writeln!(g, "  {nid}[\"{}\"]", mmd_label(m["content"].as_str().unwrap_or(""), 40));
        if m["status"].as_str() == Some("superseded") {
            old.push(nid);
        }
    }
    for (a, b) in &edges {
        let _ = writeln!(g, "  {a} -->|superseded| {b}");
    }
    if !old.is_empty() {
        let _ = writeln!(g, "  classDef old fill:#f5f5f4,stroke:#a8a29e,color:#78716c");
        let _ = writeln!(g, "  class {} old", old.join(","));
    }
    g.push_str("```");
    Some(g)
}

/// 会话谱系 Mermaid：只画有 link 的会话（fork/compact/continuation）。
/// 父会话不在本项目时间线（跨项目 fork）也入图，标签退化为会话 id；
/// 节点 id 按边首次出现顺序分配（s0…），同样确定可 diff。
fn lineage_mermaid(tl: &[Value], all_edges: Option<&Vec<Value>>) -> Option<String> {
    let mut edges: Vec<(String, String, &str)> = Vec::new(); // (父, 子, link_type)
    let legacy;
    let source = if let Some(e) = all_edges { e } else {
        legacy = tl.iter().map(|t| json!({"p": t["parent_session_id"], "c": t["session_id"], "lt": t["link_type"]})).collect::<Vec<_>>();
        &legacy
    };
    for t in source {
        if let (Some(p), Some(c), Some(lt)) = (
            t["p"].as_str(),
            t["c"].as_str(),
            t["lt"].as_str(),
        ) {
            if p != c {
                edges.push((p.to_string(), c.to_string(), lt));
            }
        }
    }
    if edges.is_empty() {
        return None;
    }
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (p, c, _) in &edges {
        for k in [p, c] {
            if !ids.contains_key(k) {
                ids.insert(k.clone(), format!("s{}", order.len()));
                order.push(k.clone());
            }
        }
    }
    let label_of = |sid: &str| -> String {
        tl.iter()
            .find(|t| t["session_id"].as_str() == Some(sid))
            .map(|t| {
                mmd_label(
                    &format!("{} {}", t["agent"].as_str().unwrap_or("—"), short(t["started_at"].as_str())),
                    32,
                )
            })
            .unwrap_or_else(|| mmd_label(sid, 24))
    };
    let mut g = String::from("```mermaid\ngraph LR\n");
    for sid in &order {
        let _ = writeln!(g, "  {}[\"{}\"]", ids[sid], label_of(sid));
    }
    for (p, c, lt) in &edges {
        let _ = writeln!(g, "  {} -->|{}| {}", ids[p], lt, ids[c]);
    }
    g.push_str("```");
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn day_bounds_havana_midnight_fallback_does_not_error() {
        // 午夜回拨的确定性回归：通过 TZ 环境切换真实时区数据库（unix）。
        // Havana 2026-11-01 01:00 回拨，本地 00:00 出现两次——earliest() 取第一次。
        // Windows 的 chrono::Local 不解析 IANA 风格 TZ（回退系统时区），门控跳过；
        // 跨平台的换算逻辑由 day_bounds_respect_local_offset（纯 FixedOffset）覆盖。
        std::env::set_var("TZ", "America/Havana");
        let r = day_bounds_utc("2026-11-01");
        std::env::remove_var("TZ");
        let (lo, hi) = r.expect("Havana 午夜回拨日不得报错（single() 会在此报错）");
        // 不钉死 tzdb 具体输出，断言结构性正确：当天/次日各一次午夜、时长 23-25h
        assert!(lo.starts_with("2026-11-01T0"), "lo={lo}");
        assert!(hi.starts_with("2026-11-02T0"), "hi={hi}");
        let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).unwrap().timestamp();
        let hours = (parse(&hi) - parse(&lo)) / 3600;
        assert!((23..=25).contains(&hours), "回拨日应 25h: {hours}");
    }

    #[test]
    fn day_bounds_respect_local_offset() {
        // 东八区的 2026-08-23 当天 = UTC 08-22T16:00Z 起
        let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let (lo, hi) = bounds_in_tz("2026-08-23", &cst).unwrap();
        assert_eq!(lo, "2026-08-22T16:00:00Z");
        assert_eq!(hi, "2026-08-23T16:00:00Z");
        // UTC 本身
        let utc = chrono::Utc;
        let (lo, hi) = bounds_in_tz("2026-08-23", &utc).unwrap();
        assert_eq!(lo, "2026-08-23T00:00:00Z");
        assert_eq!(hi, "2026-08-24T00:00:00Z");
    }
}
