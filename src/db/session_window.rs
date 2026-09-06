//! 来源定位后的有界前后文。原 read_session 的尾部语义保持不变。
use super::*;

pub fn session_window(
    conn: &Connection,
    sid: &str,
    line: Option<i64>,
    offset: Option<u32>,
    message_id: Option<i64>,
    before_compact: bool,
) -> Result<Value> {
    // 计数与页面来自同一只读快照，导入重建期间也不会拼接两版数据。
    let tx = conn.unchecked_transaction()?;
    let conn = &tx;
    let mut result = read_session(conn, sid, 0, None, before_compact)?;
    let bound = if before_compact {
        result["session"]["compact_line_no"].as_i64()
    } else {
        None
    };
    let total = result["total_messages"].as_u64().unwrap_or(0);
    let start = match offset {
        Some(n) => n as u64,
        None => {
            let (focus_line, focus_ord) = match message_id {
                Some(id) => conn.query_row("SELECT line_no, ord FROM messages WHERE id=?1 AND session_id=?2",
                    params![id, sid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?,
                None => (line.unwrap_or(i64::MAX), 0),
            };
            anyhow::ensure!(message_id.is_none() || line.is_none() || line == Some(focus_line), "source changed; search again");
            let before: u64 = conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND (line_no, ord) < (?2, ?4) AND (?3 IS NULL OR line_no < ?3)",
                params![sid, focus_line, bound, focus_ord], |r| r.get(0),
            )?;
            before.saturating_sub(75)
        }
    }.min(total.saturating_sub(1));
    let mut stmt = conn.prepare(
        "SELECT id, kind, content, timestamp, line_no FROM messages WHERE session_id = ?1
         AND (?2 IS NULL OR line_no < ?2) ORDER BY line_no, ord, id LIMIT 150 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![sid, bound, start], |r| {
        let content: String = r.get(2)?;
        Ok(json!({ "message_id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?,
            "content": content.chars().take(4000).collect::<String>(), "truncated": content.chars().count() > 4000,
            "timestamp": r.get::<_, Option<String>>(3)?, "line_no": r.get::<_, i64>(4)? }))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    result["tail_excerpt"] = json!(start > 0 || rows.len() as u64 != total);
    result["has_before"] = json!(start > 0);
    result["has_after"] = json!(start + (rows.len() as u64) < total);
    result["window_offset"] = json!(start);
    result["messages"] = json!(rows);
    Ok(result)
}

pub fn message_content(conn: &Connection, sid: &str, id: i64) -> Result<Value> {
    let content: String = conn.query_row(
        "SELECT m.content FROM messages m JOIN sessions s ON s.id=m.session_id
         WHERE m.id=?1 AND s.id=?2 AND s.deleted_at IS NULL",
        params![id, sid],
        |r| r.get(0),
    )?;
    Ok(json!({ "message_id": id, "content": content }))
}
