//! 记忆来源回填：agent 调用 save_memory 时通常不知道自己的会话 ID，记忆因此
//! 没有来源指针。导入完成后，按"同项目、正文吻合、时间相近的 save_memory 工具
//! 调用"找到真正写入这条记忆的那条消息，补上 source_session_id/source_message_id。
//! 只回填仍为空的指针；agent 显式传入的来源不覆盖。

use anyhow::Result;
use rusqlite::{params, Connection};

/// 只看最近几天创建的记忆：导入通常在一分钟内跟上，长期找不到来源的
/// （命令行或界面手动新增的）不必每轮都重扫。
const LOOKBACK_DAYS: f64 = 3.0;
/// 工具调用早于记忆写入的最大间隔（天）：长对话里调用与落库只差几秒，留足余量。
const BEFORE_DAYS: f64 = 0.25;
/// 允许工具调用时间戳略晚于记忆写入（时钟误差）。
const AFTER_DAYS: f64 = 0.01;

/// 返回本轮补上来源的记忆条数。
pub fn link_memory_sources(conn: &Connection) -> Result<u64> {
    let pending: Vec<(i64, Option<i64>, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT rowid, project_id, content, created_at FROM memories
             WHERE source_session_id IS NULL
               AND julianday(created_at) >= julianday('now') - ?1",
        )?;
        let rows = stmt.query_map(params![LOOKBACK_DAYS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let mut find = conn.prepare(
        "SELECT m.id, m.session_id FROM messages m
         WHERE m.session_id IN (
                 SELECT id FROM sessions
                 WHERE deleted_at IS NULL
                   AND (?1 IS NULL OR project_id = ?1)
                   AND (started_at IS NULL OR julianday(started_at) <= julianday(?3) + ?5)
                   AND (ended_at IS NULL OR julianday(ended_at) >= julianday(?3) - ?4))
           AND m.kind = 'tool_call'
           AND m.content LIKE '%save_memory%'
           AND instr(m.content, ?2) > 0
           AND julianday(m.timestamp) BETWEEN julianday(?3) - ?4 AND julianday(?3) + ?5
         ORDER BY abs(julianday(m.timestamp) - julianday(?3))
         LIMIT 1",
    )?;
    let mut linked = 0;
    for (rowid, project_id, content, created_at) in pending {
        let Some(needle) = needle(&content) else { continue };
        let hit: Option<(i64, String)> = find
            .query_map(params![project_id, needle, created_at, BEFORE_DAYS, AFTER_DAYS], |r| Ok((r.get(0)?, r.get(1)?)))?
            .next()
            .transpose()?;
        if let Some((message_id, session_id)) = hit {
            linked += conn.execute(
                "UPDATE memories SET source_session_id = ?1, source_message_id = ?2
                 WHERE rowid = ?3 AND source_session_id IS NULL",
                params![session_id, message_id, rowid],
            )? as u64;
        }
    }
    Ok(linked)
}

/// 正文里用来匹配工具调用参数的片段：工具调用以 JSON 或脚本文本保存，引号、
/// 反斜杠和换行在那里会被转义，所以取开头一段不含这些字符的连续文字。
fn needle(content: &str) -> Option<String> {
    const MIN: usize = 8;
    const MAX: usize = 24;
    content
        .chars()
        .take(200)
        .collect::<String>()
        .split(|c: char| matches!(c, '"' | '\\' | '\n' | '\r' | '\t' | '\''))
        .map(str::trim)
        .find(|run| run.chars().count() >= MIN)
        .map(|run| run.chars().take(MAX).collect())
}

#[cfg(test)]
mod tests {
    use super::needle;

    #[test]
    fn needle_skips_escaped_characters() {
        assert_eq!(needle("登录 token 由前端保存在 localStorage，接口通过 Authorization 头鉴权").as_deref(),
                   Some("登录 token 由前端保存在 localSto"));
        assert_eq!(needle("短\n这一行才足够长，可以用来匹配").as_deref(), Some("这一行才足够长，可以用来匹配"));
        assert_eq!(needle("太短"), None);
    }
}
