//! SQLite schema and queries. FTS5 with the trigram tokenizer so both
//! Chinese and English content are searchable by substring.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::models::NewMessage;
use crate::now_iso;

mod session_window;
pub use session_window::{session_window, message_content};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
  id         INTEGER PRIMARY KEY,
  path       TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  id            TEXT PRIMARY KEY,            -- "<agent>:<native_id>"
  agent         TEXT NOT NULL,               -- claude | codex | opencode | zcode | kimi
  native_id     TEXT NOT NULL,
  project_id    INTEGER REFERENCES projects(id),
  file_path     TEXT NOT NULL,
  cwd           TEXT,
  git_branch    TEXT,
  started_at    TEXT,
  ended_at      TEXT,
  message_count INTEGER NOT NULL DEFAULT 0,
  first_parent_uuid TEXT,
  compact_leaf_uuid TEXT,
  compact_line_no INTEGER,               -- v10 压缩点（0.3.8）：首个携带压缩摘要的请求行；之前的消息=压缩前原文
  deleted_at    TEXT,                      -- 软删回收站（§6）：非 NULL 即隐藏，恢复时清掉
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  UNIQUE(agent, native_id)
);

CREATE TABLE IF NOT EXISTS messages (
  id         INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id),
  line_no    INTEGER NOT NULL,
  ord        INTEGER NOT NULL DEFAULT 0,
  kind       TEXT NOT NULL,
  content    TEXT NOT NULL,
  timestamp  TEXT,
  uuid       TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, line_no, ord);
-- v5 的唯一索引 idx_messages_slo 只在 migrate 里创建（去重之后）：老库若有
-- 并发 bug 留下的重复行，放这里会在 migrate 前就把 open() 炸掉（codex 五审）。
CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(timestamp);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  content,
  content='messages',
  content_rowid='id',
  tokenize='trigram'
);
-- 1.0.1 轻量索引：工具输出（tool_result）从对话层索引拆出，落到本表。
-- 本表常驻（空表开销可忽略），是否建索引由工具层触发器的存在与否表达：
-- 轻量态（默认）无触发器 + 空表；全量态触发器常在 + 全量数据。查询侧无条件
-- UNION 本表，空表自然无结果，读路径无需感知模式。
CREATE VIRTUAL TABLE IF NOT EXISTS messages_tools_fts USING fts5(
  content,
  content='messages',
  content_rowid='id',
  tokenize='trigram'
);

-- Ingest progress per source file (crash-safe: advanced in the same
-- transaction as the message inserts).
CREATE TABLE IF NOT EXISTS source_files (
  path            TEXT NOT NULL,
  agent           TEXT NOT NULL,
  imported_bytes  INTEGER NOT NULL DEFAULT 0,
  line_count      INTEGER NOT NULL DEFAULT 0,
  -- OpenCode 专用：毫秒游标的决胜值（同毫秒最后一条 part 的 id）。
  cursor_text     TEXT,
  -- codex 新格式标志（v7）：本文件出现过 response_item 行 → event_msg 用户/
  -- 助手消息一律视为重复副本抑制（跨增量 chunk 依然成立，见 codex adapter）
  saw_response_item INTEGER NOT NULL DEFAULT 0,
  updated_at      TEXT NOT NULL,
  -- v8：主键 (agent, path)——同一路径可被多个 agent 登记为采集根，各自持游标
  PRIMARY KEY (agent, path)
);

-- Vault manifest: which content-addressed line objects make up each session.
CREATE TABLE IF NOT EXISTS vault_lines (
  session_id TEXT NOT NULL,
  line_no    INTEGER NOT NULL,
  hash       TEXT NOT NULL,
  PRIMARY KEY (session_id, line_no)
);

CREATE TABLE IF NOT EXISTS handoffs (
  id            INTEGER PRIMARY KEY,
  project_id    INTEGER NOT NULL REFERENCES projects(id),
  session_id    TEXT,
  title         TEXT NOT NULL DEFAULT '',
  done          TEXT NOT NULL DEFAULT '',
  state         TEXT NOT NULL DEFAULT '',
  decisions     TEXT NOT NULL DEFAULT '',
  files_changed TEXT NOT NULL DEFAULT '',
  open_issues   TEXT NOT NULL DEFAULT '',
  next_steps    TEXT NOT NULL DEFAULT '',
  created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_handoffs_project ON handoffs(project_id, id DESC);

-- Memory: curated, provenance-tracked, lifecycle-managed knowledge.
-- scope: global | project | session. project_id NULL means global.
CREATE TABLE IF NOT EXISTS memories (
  id                TEXT PRIMARY KEY,
  project_id        INTEGER REFERENCES projects(id),
  scope             TEXT NOT NULL,
  type              TEXT NOT NULL,
  content           TEXT NOT NULL,
  status            TEXT NOT NULL DEFAULT 'confirmed',  -- suggested | confirmed | superseded | archived
  source_session_id TEXT,
  source_message_id INTEGER,
  superseded_by     TEXT,
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memories_project ON memories(project_id, status);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  content,
  content='memories',
  content_rowid='rowid',
  tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
  INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;

-- Session <-> artifact association (files written by the agent).
CREATE TABLE IF NOT EXISTS session_artifacts (
  id         INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  project_id INTEGER REFERENCES projects(id),
  path       TEXT NOT NULL,
  tool       TEXT,
  created_at TEXT NOT NULL,
  UNIQUE(session_id, path)
);
CREATE INDEX IF NOT EXISTS idx_artifacts_project ON session_artifacts(project_id);

-- Session lineage: fork / continuation / compact / subagent edges.
CREATE TABLE IF NOT EXISTS session_links (
  child_session_id  TEXT NOT NULL,
  parent_session_id TEXT NOT NULL,
  link_type         TEXT NOT NULL,
  via_uuid          TEXT,
  created_at        TEXT NOT NULL,
  PRIMARY KEY (child_session_id, parent_session_id, link_type)
);

-- Every uuid sighted in a session file, including non-content lines.
-- Lineage pointers often reference lines that never become messages.
CREATE TABLE IF NOT EXISTS session_uuids (
  session_id TEXT NOT NULL,
  line_no    INTEGER NOT NULL,
  uuid       TEXT NOT NULL,
  PRIMARY KEY (session_id, line_no, uuid)
);
CREATE INDEX IF NOT EXISTS idx_uuids_uuid ON session_uuids(uuid);

CREATE INDEX IF NOT EXISTS idx_messages_uuid ON messages(uuid) WHERE uuid IS NOT NULL;

-- Native agent memory files (Claude auto memory MEMORY.md, ~/.codex/AGENTS.md…):
-- whole-file snapshots with revision history (DESIGN-0.3 §2). These files are
-- rewritten in place, so the vault object stores the file's full bytes.
CREATE TABLE IF NOT EXISTS memory_files (
  id INTEGER PRIMARY KEY,
  agent TEXT NOT NULL,                  -- claude / codex / manual
  scope TEXT NOT NULL DEFAULT 'global', -- global | project:<project root>
  path TEXT NOT NULL,                   -- absolute path of the live file
  current_hash TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(agent, path)
);
CREATE TABLE IF NOT EXISTS memory_revisions (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES memory_files(id),
  hash TEXT NOT NULL,                   -- vault object hash (dedups unchanged content)
  size INTEGER NOT NULL,
  captured_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_revisions_file ON memory_revisions(file_id, captured_at);

-- Latest-revision content only (rowid = memory_files.id), manually maintained
-- (delete + reinsert on change). 有意取舍：历史修订可浏览不可全文搜索。
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
  content,
  tokenize='trigram'
);

-- 物理清除的墓碑（v6）：purge 过的源文件不再被 import 复活（游标清了也会
-- 完整重导，墓碑是唯一的"不再要了"标记）。
CREATE TABLE IF NOT EXISTS purged_sources (
  path      TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  purged_at TEXT NOT NULL
);
-- GC 待回收队列（v6）：候选对象先入队再逐个归档（rename 进备份目录），
-- 中途失败重跑 sweep 即可清尾巴——unlink 不可重入的问题由此根治。
CREATE TABLE IF NOT EXISTS gc_pending (
  hash     TEXT PRIMARY KEY,
  added_at TEXT NOT NULL
);
-- 本地使用侧指标（DESIGN-0.3 §11）：仅命令/工具名 + 时间戳，不外传、不上报。
CREATE TABLE IF NOT EXISTS usage_log (
  id     INTEGER PRIMARY KEY,
  ts     TEXT NOT NULL,
  source TEXT NOT NULL,  -- cli | mcp | app
  name   TEXT NOT NULL   -- command or tool name
);
CREATE INDEX IF NOT EXISTS idx_usage_log_ts ON usage_log(ts);
"#;

/// 消息触发器与 SCHEMA 分离：触发器按 kind 把消息分流进对话层/工具层两张
/// FTS 表。对话层触发器常驻（open 每次确保存在）；工具层触发器仅全量态存在
/// （TOOL_TRIGGERS_SQL，enable 显式建、disable/open 收敛显式删）。迁移 v12
/// 要把旧库的无 WHEN 触发器换成本文，共用这一份 DDL 防止两处漂移。
pub const TRIGGERS_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages
  WHEN NEW.kind <> 'tool_result' BEGIN
  INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages
  WHEN OLD.kind <> 'tool_result' BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
"#;

/// 工具层触发器：只在全量索引态存在。轻量态下 tool_result 的插入/删除
/// 不碰任何 FTS；切全量时 enable_tool_index 先建触发器再同事务回填，崩溃
/// 整体回滚，不会留下半满索引。
pub const TOOL_TRIGGERS_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS messages_tools_ai AFTER INSERT ON messages
  WHEN NEW.kind = 'tool_result' BEGIN
  INSERT INTO messages_tools_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS messages_tools_ad AFTER DELETE ON messages
  WHEN OLD.kind = 'tool_result' BEGIN
  INSERT INTO messages_tools_fts(messages_tools_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
"#;

pub fn open(home: &Path) -> Result<Connection> {
    std::fs::create_dir_all(home)?;
    let conn = Connection::open(home.join("yourmem.db"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(TRIGGERS_SQL)?;
    migrate(&conn)?;
    // 轻量收敛：config 期望轻量而工具索引还在（enable 后 config 写回失败的
    // 崩溃窗口、或用户手改 config）——收掉。期望全量时不自动重建（分钟级
    // 操作，显式走 index enable-tools），由 index_status 向 UI 报告未生效。
    if !tool_index_wanted(home) && tool_index_enabled(&conn)? {
        disable_tool_index(&conn)?;
    }
    Ok(conn)
}

/// Current schema version, stamped into `PRAGMA user_version` by migrate().
/// Bump this (and add a migration step below) whenever the schema changes;
/// bundle manifests record it (DESIGN-0.3 §5.1 `schema_version`).
pub const SCHEMA_VERSION: i32 = 12;

/// Idempotent column additions for databases created by older versions.
/// `user_version` drives the fast path: a database already stamped with the
/// current SCHEMA_VERSION skips all checks. Anything below (in practice: 0,
/// i.e. created before versioning existed) gets the full idempotent pass.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let has_col = |table: &str, col: &str| -> Result<bool> {
        let cols: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(cols.iter().any(|c| c == col))
    };
    for (table, col, ddl) in [
        ("sessions", "first_parent_uuid", "ALTER TABLE sessions ADD COLUMN first_parent_uuid TEXT"),
        ("sessions", "compact_leaf_uuid", "ALTER TABLE sessions ADD COLUMN compact_leaf_uuid TEXT"),
        // OpenCode 增量游标的决胜值：同毫秒下最后一条 part 的 id。
        ("source_files", "cursor_text", "ALTER TABLE source_files ADD COLUMN cursor_text TEXT"),
        // 0.3.2 软删回收站（§6）：NULL = 活跃；非 NULL = 在回收站，全端隐藏。
        ("sessions", "deleted_at", "ALTER TABLE sessions ADD COLUMN deleted_at TEXT"),
        // v7：codex 新格式标志——event_msg 抑制要跨增量 chunk（见 codex adapter）。
        ("source_files", "saw_response_item", "ALTER TABLE source_files ADD COLUMN saw_response_item INTEGER NOT NULL DEFAULT 0"),
        // v10（0.3.8）：压缩点——zcode 同文件内压缩的边界行号（首个携带 harness
        // 摘要前缀的请求行）；NULL = 该会话未压缩过。源无关设计，其余 adapter
        // 样本到位后只加检测函数。
        ("sessions", "compact_line_no", "ALTER TABLE sessions ADD COLUMN compact_line_no INTEGER"),
        // v11：项目归档（"废弃"）——NULL = 活跃；非 NULL = 用户废弃，列表全端
        // 隐藏，会话与 vault 资产原样保留；导入不复活（upsert_project 不碰本列）。
        ("projects", "archived_at", "ALTER TABLE projects ADD COLUMN archived_at TEXT"),
    ] {
        if !has_col(table, col)? {
            // TOCTOU 防御（codex 复现）：两个进程同时初始化全新库时，都可能
            // 走到这里——后提交的 ALTER 撞 duplicate column。列已存在即视为
            // 成功（幂等），其余错误照常上抛。
            if let Err(e) = conn.execute_batch(ddl) {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }
    }
    // v5（0.3.4）：messages 唯一索引。历史库可能已有并发 bug 留下的重复行，
    // 先去重（保留每组最小 rowid，FTS 触发器联动清理）再建索引；建不起来
    // 就让错误显式冒出来——静默跳过等于把数据问题藏进下一次双插。
    // (a) 指针重定向：memories.source_message_id 指向将被删除的重复行时，
    //     改指同组保留行（保留行 = 组内最小 rowid；无 FK，必须显式维护）。
    conn.execute_batch(
        "UPDATE memories SET source_message_id = (
           SELECT MIN(keep.rowid) FROM messages keep
           JOIN messages dup ON dup.session_id = keep.session_id
               AND dup.line_no = keep.line_no AND dup.ord = keep.ord
           WHERE keep.rowid = (SELECT MIN(k2.rowid) FROM messages k2
                               WHERE k2.session_id = dup.session_id
                                 AND k2.line_no = dup.line_no AND k2.ord = dup.ord)
             AND dup.rowid = memories.source_message_id
         ) WHERE source_message_id IS NOT NULL
           AND source_message_id IN (
             SELECT rowid FROM messages WHERE rowid NOT IN (
               SELECT MIN(rowid) FROM messages GROUP BY session_id, line_no, ord))",
    )?;
    // (b) 删重复（保留每组最小 rowid；FTS 由 ad 触发器联动）
    conn.execute_batch(
        "DELETE FROM messages WHERE rowid NOT IN (
           SELECT MIN(rowid) FROM messages GROUP BY session_id, line_no, ord)",
    )?;
    // (c) 会话计数重算为真相
    conn.execute_batch(
        "UPDATE sessions SET message_count =
           (SELECT COUNT(*) FROM messages WHERE messages.session_id = sessions.id)",
    )?;
    if let Err(e) = conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_slo ON messages(session_id, line_no, ord)",
    ) {
        let msg = e.to_string();
        if !msg.contains("already exists") {
            return Err(e.into());
        }
    }
    // v8：source_files 主键 path → (agent, path)。同一目录可被多个 agent 登记
    // 为采集根（extra_roots 允许），单 path 主键下第二个 agent 会吃到第一个
    // 的游标而整体漏采（codex 评审 blocker）。SQLite 改不了 PK，走表重建；
    // 每步幂等，中断重入安全（旧表在则重灌，仅新表在则改名收尾）。
    {
        let table_exists = |name: &str| -> Result<bool> {
            let n: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{name}'"),
                [],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        };
        let composite_pk = || -> Result<bool> {
            let mut st = conn.prepare("PRAGMA table_info(source_files)")?;
            let pks: Vec<i64> = st.query_map([], |r| r.get::<_, i64>(5))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(pks.iter().filter(|&&p| p > 0).count() >= 2)
        };
        let (old_exists, new_exists) = (table_exists("source_files")?, table_exists("source_files_v8")?);
        if old_exists && !composite_pk()? {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS source_files_v8 (
                   path TEXT NOT NULL, agent TEXT NOT NULL,
                   imported_bytes INTEGER NOT NULL DEFAULT 0,
                   line_count INTEGER NOT NULL DEFAULT 0,
                   cursor_text TEXT,
                   saw_response_item INTEGER NOT NULL DEFAULT 0,
                   updated_at TEXT NOT NULL,
                   PRIMARY KEY (agent, path)
                 );
                 INSERT OR REPLACE INTO source_files_v8
                   (path, agent, imported_bytes, line_count, cursor_text, saw_response_item, updated_at)
                 SELECT path, agent, imported_bytes, line_count, cursor_text,
                        COALESCE(saw_response_item, 0), updated_at
                 FROM source_files;
                 DROP TABLE source_files;
                 ALTER TABLE source_files_v8 RENAME TO source_files;",
            )?;
        } else if !old_exists && new_exists {
            // 上次迁移在 DROP 与 RENAME 之间中断：收尾
            conn.execute_batch("ALTER TABLE source_files_v8 RENAME TO source_files;")?;
        }
    }
    // v9：修复 v8 重建漏掉 cursor_text 的库（重建表定义当时没带这列，真实旧库
    // 升 v8 后列与数据一起消失——新库靠 SCHEMA+ALTER 双保险不受影响）。幂等补列。
    // 丢过的决胜值影响有限：opencode 同毫秒 part 可能重扫一次，OR IGNORE 防双录。
    if !has_col("source_files", "cursor_text")? {
        if let Err(e) = conn.execute_batch("ALTER TABLE source_files ADD COLUMN cursor_text TEXT") {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(e.into());
            }
        }
    }
    // v12（1.0.1）轻量索引：messages_fts 收窄为对话层（kind <> 'tool_result'），
    // 工具输出拆到 messages_tools_fts（默认空表，可插拔，见 open/enable_tool_index）。
    // 旧库的 messages_ai/ad 无 WHEN 过滤（全量单表索引）——换触发器 + 整表重建
    // 对话层索引。三步同事务：崩溃整体回滚到旧形态，下次 open 重走。重建量
    // ≈ 非工具输出行数（真实库约 14 万行，分钟内）；磁盘回收交给 index compact。
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DROP TRIGGER IF EXISTS messages_ai;
         DROP TRIGGER IF EXISTS messages_ad;
         INSERT INTO messages_fts(messages_fts) VALUES('delete-all');
         INSERT INTO messages_fts(rowid, content) SELECT id, content FROM messages WHERE kind <> 'tool_result';
         COMMIT;",
    )?;
    conn.execute_batch(TRIGGERS_SQL)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

// ---------------------------------------------------------------- projects

/// Resolve a cwd to its project root (nearest ancestor containing `.git`,
/// or the cwd itself when no repository is found).
pub fn project_root_for(cwd: &str) -> (String, String) {
    let mut p = Path::new(cwd);
    let mut root = None;
    loop {
        if p.join(".git").exists() {
            root = Some(p);
            break;
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    let p = root.unwrap_or(Path::new(cwd));
    let path = p.to_string_lossy().to_string();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    (path, name)
}

pub fn upsert_project(conn: &Connection, path: &str, name: &str) -> Result<i64> {
    let now = now_iso();
    conn.execute(
        "INSERT INTO projects(path, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(path) DO UPDATE SET updated_at = excluded.updated_at",
        params![path, name, now],
    )?;
    let id: i64 = conn.query_row("SELECT id FROM projects WHERE path = ?1", params![path], |r| r.get(0))?;
    Ok(id)
}

// ---------------------------------------------------------------- sessions

pub fn upsert_session(
    conn: &Connection,
    id: &str,
    agent: &str,
    native_id: &str,
    project_id: Option<i64>,
    file_path: &str,
    meta: &crate::models::SessionMetaPatch,
    new_messages: u64,
) -> Result<()> {
    let now = now_iso();
    conn.execute(
        "INSERT INTO sessions(id, agent, native_id, project_id, file_path, cwd, git_branch,
                              started_at, ended_at, message_count, first_parent_uuid,
                              compact_leaf_uuid, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13)
         ON CONFLICT(agent, native_id) DO UPDATE SET
           project_id   = COALESCE(sessions.project_id, excluded.project_id),
           cwd          = COALESCE(sessions.cwd, excluded.cwd),
           git_branch   = COALESCE(sessions.git_branch, excluded.git_branch),
           file_path    = excluded.file_path, -- 源文件会移动；purge 墓碑按 path 立碑，旧路径会让已清会话复活
           started_at   = COALESCE(sessions.started_at, excluded.started_at),
           ended_at     = MAX(COALESCE(sessions.ended_at,''), COALESCE(excluded.ended_at,'')),
           message_count= sessions.message_count + excluded.message_count,
           first_parent_uuid = COALESCE(sessions.first_parent_uuid, excluded.first_parent_uuid),
           compact_leaf_uuid = COALESCE(sessions.compact_leaf_uuid, excluded.compact_leaf_uuid),
           updated_at   = excluded.updated_at",
        params![
            id, agent, native_id, project_id, file_path,
            meta.cwd, meta.git_branch, meta.started_at, meta.ended_at,
            new_messages as i64, meta.first_parent_uuid, meta.compact_leaf_uuid, now
        ],
    )?;
    Ok(())
}

pub fn insert_messages(conn: &Connection, session_id: &str, msgs: &[NewMessage]) -> Result<()> {
    // OR IGNORE：并发/重放路径的纵深防御——(session,line,ord) 已存在则跳过
    // （唯一索引 idx_messages_slo 兜底；FTS 触发器只在实际插入时触发）。
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO messages(session_id, line_no, ord, kind, content, timestamp, uuid)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )?;
    for m in msgs {
        stmt.execute(params![
            session_id,
            m.line_no as i64,
            m.ord as i64,
            m.kind.as_str(),
            m.content,
            m.timestamp,
            m.uuid
        ])?;
    }
    Ok(())
}

/// 裁定（DESIGN-0.3 §8）：**有意非事务**。本函数只从 ingest 的截断/替换
/// 重导路径与 bundle prune 调用；每条语句幂等，中途崩溃留下的残余会在下一次
/// 重导入前被再次调用清掉（自愈）。加事务没有收益，维持现状。
pub fn delete_session_data(conn: &Connection, session_id: &str) -> Result<()> {
    // messages 的 rowid 删除后会被新行复用：先把指向本会话消息的来源指针置
    // NULL（行号口径已变，置空比重映射诚实——5f6de93 修的正是这类错挂）；
    // artifacts/links 同清，对齐 purge_session，否则截断重导会复活已消失的
    // artifact（insert_artifacts 走 OR IGNORE 挡不住）。
    conn.execute(
        "UPDATE memories SET source_message_id = NULL WHERE source_session_id = ?1 AND source_message_id IS NOT NULL",
        params![session_id],
    )?;
    conn.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id])?;
    conn.execute("DELETE FROM vault_lines WHERE session_id = ?1", params![session_id])?;
    conn.execute("DELETE FROM session_uuids WHERE session_id = ?1", params![session_id])?;
    conn.execute("DELETE FROM session_artifacts WHERE session_id = ?1", params![session_id])?;
    conn.execute(
        "DELETE FROM session_links WHERE child_session_id = ?1 OR parent_session_id = ?1",
        params![session_id],
    )?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
    Ok(())
}

pub fn insert_uuid_sightings(conn: &Connection, session_id: &str, sightings: &[crate::models::UuidSighting]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO session_uuids(session_id, line_no, uuid) VALUES (?1,?2,?3)",
    )?;
    for s in sightings {
        stmt.execute(params![session_id, s.line_no as i64, s.uuid])?;
    }
    Ok(())
}

// ------------------------------------------------------------ source files

pub struct SourceFileState {
    /// JSONL adapter：字节 offset；OpenCode adapter：借用为 time_created 毫秒游标。
    pub imported_bytes: u64,
    pub line_count: u64,
    /// OpenCode 专用：毫秒游标的决胜值（同毫秒最后一条 part 的 id）。
    pub cursor_text: Option<String>,
    /// codex 专用：本文件出现过 response_item（新格式）——一旦为真，event_msg
    /// 用户/助手消息在后续所有 chunk 中都按重复副本抑制。
    pub saw_response_item: bool,
}

/// 游标按 (agent, path) 取（v8）：同一路径可被多个 agent 登记为采集根，
/// 单 path 键会让第二个 agent 吃到第一个的游标直接漏采（codex 评审 blocker）
pub fn source_file_state(conn: &Connection, agent: &str, path: &str) -> Result<Option<SourceFileState>> {
    let row = conn
        .query_row(
            "SELECT imported_bytes, line_count, cursor_text, saw_response_item FROM source_files
             WHERE agent = ?1 AND path = ?2",
            params![agent, path],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?)),
        )
        .optional()?;
    Ok(row.map(|(b, l, c, s)| SourceFileState { imported_bytes: b as u64, line_count: l as u64, cursor_text: c, saw_response_item: s != 0 }))
}

pub fn update_source_file(conn: &Connection, path: &str, agent: &str, bytes: u64, lines: u64, cursor_text: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO source_files(path, agent, imported_bytes, line_count, cursor_text, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(agent, path) DO UPDATE SET imported_bytes=excluded.imported_bytes,
           line_count=excluded.line_count, cursor_text=excluded.cursor_text, updated_at=excluded.updated_at",
        params![path, agent, bytes as i64, lines as i64, cursor_text, now_iso()],
    )?;
    Ok(())
}

pub fn delete_source_file(conn: &Connection, agent: &str, path: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM source_files WHERE agent = ?1 AND path = ?2",
        params![agent, path],
    )?;
    Ok(())
}

// ------------------------------------------------------------------ search

pub struct SearchOpts {
    pub query: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    pub limit: u32,
}

/// LIKE 模式转义：用户输入里的 %/_/\ 按字面匹配（配套 `ESCAPE '\'` 子句）。
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// FTS5 trigram search, falling back to LIKE for tokens shorter than
/// 3 chars (trigram cannot match those — common for 1-2 char CJK words).
pub fn search(conn: &Connection, opts: &SearchOpts) -> Result<Vec<Value>> {
    let use_fts = opts.query.split_whitespace().all(|t| t.chars().count() >= 3) && !opts.query.trim().is_empty();
    let project = opts.project.as_deref();
    let agent = opts.agent.as_deref();
    let kind = opts.kind.as_deref();
    let limit = opts.limit as i64;

    let base_select = r#"
        SELECT m.id, m.session_id, s.agent, p.name, p.path, m.kind, m.timestamp, m.content, m.line_no
        FROM messages m
        JOIN sessions s ON s.id = m.session_id
        LEFT JOIN projects p ON p.id = s.project_id
    "#;

    let rows = if use_fts {
        let match_q = fts_query(&opts.query);
        let sql = format!(
            "{base_select}
             WHERE m.id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1
                            UNION
                            SELECT rowid FROM messages_tools_fts WHERE messages_tools_fts MATCH ?1)
               AND s.deleted_at IS NULL
               AND (?2 IS NULL OR p.name = ?2 OR p.path LIKE '%'||?2||'%')
               AND (?3 IS NULL OR s.agent = ?3)
               AND (?4 IS NULL OR m.kind = ?4)
             ORDER BY m.timestamp DESC LIMIT ?5"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![match_q, project, agent, kind, limit], hit_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        // 多词回退保持 AND 语义（与 FTS 分支一致）：整串塞一个 LIKE 要求词项
        // 字面相邻，"盆地 变换" 这类查询结果几乎必空。空查询沿用原语义（全命中）
        let tokens: Vec<String> = if opts.query.trim().is_empty() {
            vec![]
        } else {
            opts.query.split_whitespace().map(|t| format!("%{}%", like_escape(t))).collect()
        };
        let n = tokens.len();
        let content_clause = if n == 0 {
            "1=1".to_string()
        } else {
            (1..=n).map(|i| format!("m.content LIKE ?{i} ESCAPE '\\'")).collect::<Vec<_>>().join(" AND ")
        };
        let sql = format!(
            "{base_select}
             WHERE {content_clause}
               AND s.deleted_at IS NULL
               AND (?{pe} IS NULL OR p.name = ?{p} OR p.path LIKE '%'||?{pe}||'%' ESCAPE '\\')
               AND (?{a} IS NULL OR s.agent = ?{a})
               AND (?{k} IS NULL OR m.kind = ?{k})
             ORDER BY m.timestamp DESC LIMIT ?{l}",
            p = n + 1,
            pe = n + 2,
            a = n + 3,
            k = n + 4,
            l = n + 5,
        );
        use rusqlite::types::Value as SqlValue;
        let opt = |o: Option<&str>| o.map(|s| SqlValue::from(s.to_string())).unwrap_or(SqlValue::Null);
        let mut pv: Vec<SqlValue> = tokens.into_iter().map(SqlValue::from).collect();
        pv.extend([
            opt(project),
            project.map(|s| SqlValue::from(like_escape(s))).unwrap_or(SqlValue::Null),
            opt(agent),
            opt(kind),
            SqlValue::from(limit),
        ]);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(pv), hit_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    Ok(rows
        .into_iter()
        .map(|(id, session_id, agent, pname, ppath, kind, ts, content, line_no)| {
            json!({
                "message_id": id,
                "line_no": line_no,
                "session_id": session_id,
                "agent": agent,
                "project": pname,
                "project_path": ppath,
                "kind": kind,
                "timestamp": ts,
                "snippet": snippet_for(&content, &opts.query),
            })
        })
        .collect())
}

type HitRow = (i64, String, String, Option<String>, Option<String>, String, Option<String>, String, i64);

fn hit_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<HitRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
}

/// Quote each whitespace-separated token and AND them together.
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

// -------------------------------------------------------- 轻量索引（1.0.1）
// 工具输出（tool_result）的全文索引默认关闭：真实库上 trigram 索引是 db 的
// 第一大成分（1.79GB / 65%，tool_result 正文又占其中 77%）。模式表达为
// 「工具层触发器是否存在」：轻量态触发器不在 + 工具表为空；全量态触发器在 +
// 全量数据。config.json 的 index_tool_output 是期望态（缺省 false），db 向它
// 收敛：轻量方向 open 时自动收敛（瞬时），全量方向必须显式重建（分钟级）。

/// config.json 里的期望态键。
pub const CFG_TOOL_INDEX: &str = "index_tool_output";

fn tool_index_wanted(home: &Path) -> bool {
    crate::ingest::read_config(home)[CFG_TOOL_INDEX].as_bool().unwrap_or(false)
}

/// 当前是否处于全量索引态（以工具层触发器的存在为准）。
pub fn tool_index_enabled(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'messages_tools_ai'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 切全量：建工具层触发器 + 同事务回填全部 tool_result，崩溃整体回滚。
/// 大库分钟级（真实库 8 万行 / 511MB 正文 → 约 1.4GB 索引）。
pub fn enable_tool_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         {TOOL_TRIGGERS_SQL}
         INSERT INTO messages_tools_fts(rowid, content) SELECT id, content FROM messages WHERE kind = 'tool_result';
         COMMIT;"
    ))?;
    Ok(())
}

/// 切轻量：删工具层触发器 + 清空工具表（'delete-all' 只清索引不碰 messages），
/// 同事务，崩溃回滚。磁盘回收靠调用方跟进 VACUUM。
pub fn disable_tool_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DROP TRIGGER IF EXISTS messages_tools_ai;
         DROP TRIGGER IF EXISTS messages_tools_ad;
         INSERT INTO messages_tools_fts(messages_tools_fts) VALUES('delete-all');
         COMMIT;",
    )?;
    Ok(())
}

/// 开关入口（CLI 与桌面共用）：切换 + 回写 config + 回收磁盘（关方向跟
/// VACUUM——不回收的话文件体积不变，用户看不到任何效果）。返回切换后的状态。
pub fn set_tool_index(home: &Path, conn: &Connection, full: bool) -> Result<Value> {
    if full {
        enable_tool_index(conn)?;
    } else {
        disable_tool_index(conn)?;
        conn.execute_batch("VACUUM")?;
    }
    let mut cfg = crate::ingest::read_config(home);
    cfg[CFG_TOOL_INDEX] = json!(full);
    crate::ingest::write_config(home, &cfg)?;
    index_status(conn)
}

/// 索引模式与体量状态（CLI `index status` 与设置页共用）。
pub fn index_status(conn: &Connection) -> Result<Value> {
    let full = tool_index_enabled(conn)?;
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
    let page_count: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let freelist: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let tool_msgs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE kind = 'tool_result'", [], |r| r.get(0))?;
    // 注意：external content 的 FTS5 表 COUNT(*) 会穿透到内容表（= messages 行数），
    // 已索引行数必须数 docsize 影子表。
    let indexed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_tools_fts_docsize", [], |r| r.get(0))?;
    Ok(json!({
        "tool_index_full": full,
        "tool_messages": tool_msgs,
        "indexed_tool_rows": indexed,
        "db_bytes": page_size * page_count,
        "free_bytes": page_size * freelist,
    }))
}

/// 核心数据与备份目录的占用和实际位置，供设置页明确区分两类存储。
/// objects 是几十万碎文件，调用方须放阻塞线程。
pub fn storage_usage(home: &Path) -> Value {
    let dir_bytes = |p: &Path| -> u64 {
        walkdir::WalkDir::new(p)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    };
    let objects = crate::vault::objects_root(home);
    let backups = crate::backups_dir(home);
    json!({
        "data_dir": home.display().to_string(),
        "objects_dir": objects.display().to_string(),
        "backups_dir": backups.display().to_string(),
        "db_bytes": std::fs::metadata(home.join("yourmem.db")).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(home.join("yourmem.db-wal")).map(|m| m.len()).unwrap_or(0),
        "objects_bytes": dir_bytes(&objects),
        "backups_bytes": dir_bytes(&backups),
    })
}

/// A short window around the first match, for display / MCP responses.
fn snippet_for(content: &str, query: &str) -> String {
    let needle = query.split_whitespace().next().unwrap_or(query);
    let lower = content.to_lowercase();
    let pos = lower.find(&needle.to_lowercase());
    let (start, _) = match pos {
        Some(byte_pos) => {
            let char_pos = lower[..byte_pos].chars().count();
            (char_pos.saturating_sub(80), char_pos)
        }
        None => (0, 0),
    };
    let chars: Vec<char> = content.chars().collect();
    let end = (start + 240).min(chars.len());
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.extend(chars[start..end].iter());
    if end < chars.len() {
        s.push('…');
    }
    s
}

// ---------------------------------------------------------------- projects

pub fn list_projects(conn: &Connection) -> Result<Vec<Value>> {
    list_projects_q(conn, false)
}

/// 归档项目视图（Projects 页折叠区 / CLI projects archive 的核对清单）。
pub fn list_archived_projects(conn: &Connection) -> Result<Vec<Value>> {
    list_projects_q(conn, true)
}

fn list_projects_q(conn: &Connection, archived: bool) -> Result<Vec<Value>> {
    let filter = if archived { "p.archived_at IS NOT NULL" } else { "p.archived_at IS NULL" };
    let mut stmt = conn.prepare(&format!(
        "SELECT p.id, p.name, p.path,
                COUNT(DISTINCT s.id) AS sessions,
                COALESCE(SUM(s.message_count), 0) AS messages,
                MAX(s.ended_at) AS last_activity,
                (SELECT h.created_at FROM handoffs h WHERE h.project_id = p.id ORDER BY h.id DESC LIMIT 1) AS last_handoff,
                GROUP_CONCAT(DISTINCT s.agent) AS agents,
                p.archived_at
         FROM projects p
         LEFT JOIN sessions s ON s.project_id = p.id AND s.deleted_at IS NULL
         WHERE {filter}
         GROUP BY p.id ORDER BY last_activity DESC"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "name": r.get::<_, String>(1)?,
            "path": r.get::<_, String>(2)?,
            "sessions": r.get::<_, i64>(3)?,
            "messages": r.get::<_, i64>(4)?,
            "last_activity": r.get::<_, Option<String>>(5)?,
            "last_handoff": r.get::<_, Option<String>>(6)?,
            "agents": r.get::<_, Option<String>>(7)?
                .map(|a| a.split(',').map(str::to_string).collect::<Vec<String>>()).unwrap_or_default(),
            "archived_at": r.get::<_, Option<String>>(8)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// 手动登记项目文件夹（Projects 页"添加" / CLI projects add）：只建项目行，
/// 不涉及采集（采集根是另一回事，走 agents add 的 extra_roots）。
/// 路径由调用方做 ~ 展开与存在性校验；重复登记幂等返回现有 id，
/// 重登记已归档路径 = 顺带恢复（用户明确表示要用了）。
pub fn add_project(conn: &Connection, path: &str) -> Result<(i64, bool)> {
    let trimmed = path.trim_end_matches('/');
    let name = std::path::Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("cannot derive project name from path: {path}"))?;
    if let Some((id, was_archived)) = conn
        .query_row(
            "SELECT id, archived_at FROM projects WHERE path = ?1",
            params![trimmed],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?.is_some())),
        )
        .optional()?
    {
        if was_archived {
            conn.execute(
                "UPDATE projects SET archived_at = NULL WHERE id = ?1",
                params![id],
            )?;
        }
        return Ok((id, false));
    }
    let now = now_iso();
    conn.execute(
        "INSERT INTO projects(path, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![trimmed, name, now],
    )?;
    Ok((conn.last_insert_rowid(), true))
}

/// 项目归档（"废弃"）：软标记可恢复。列表全端隐藏（含 stats 计数），
/// 会话与 vault 资产原样保留；导入不复活——upsert_project 的
/// ON CONFLICT 只碰 updated_at。幂等防呆同 set_session_deleted。
pub fn set_project_archived(conn: &Connection, id: i64, archived: bool) -> Result<()> {
    let n = if archived {
        conn.execute(
            "UPDATE projects SET archived_at = ?2 WHERE id = ?1 AND archived_at IS NULL",
            params![id, now_iso()],
        )?
    } else {
        conn.execute(
            "UPDATE projects SET archived_at = NULL WHERE id = ?1 AND archived_at IS NOT NULL",
            params![id],
        )?
    };
    let state = if archived { "not found or already archived" } else { "not archived" };
    anyhow::ensure!(n > 0, "project {state}: {id}");
    Ok(())
}

/// Resolve a project by exact name, else path/name substring.
/// `None` matches a project whose path contains the given cwd, or the only project.
pub fn resolve_project(conn: &Connection, ident: Option<&str>, cwd: Option<&str>) -> Result<Option<(i64, String, String)>> {
    if let Some(ident) = ident {
        let row = conn
            .query_row(
                "SELECT id, name, path FROM projects
                 WHERE name = ?1 OR path = ?1
                    OR path LIKE '%'||?2||'%' ESCAPE '\\' OR name LIKE '%'||?2||'%' ESCAPE '\\'
                 ORDER BY LENGTH(path) LIMIT 1",
                params![ident, like_escape(ident)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        return Ok(row);
    }
    if let Some(cwd) = cwd {
        let (root, _) = project_root_for(cwd);
        let row = conn
            .query_row(
                "SELECT id, name, path FROM projects WHERE path = ?1 OR ?1 LIKE path||'/%' ORDER BY LENGTH(path) DESC LIMIT 1",
                params![root],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if row.is_some() {
            return Ok(row);
        }
    }
    // fall back to the single most recently active project.
    // deleted_at 过滤放 JOIN 条件而非 WHERE：回收站会话不再推高"最近活跃"，
    // 但只剩回收站会话的项目仍保留候选资格（与原语义一致，只是排到最后）
    let row = conn
        .query_row(
            "SELECT p.id, p.name, p.path FROM projects p
             LEFT JOIN sessions s ON s.project_id = p.id AND s.deleted_at IS NULL
             GROUP BY p.id ORDER BY MAX(s.ended_at) DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(row)
}

pub fn project_context(conn: &Connection, project_id: i64) -> Result<Value> {
    let (name, path): (String, String) =
        conn.query_row("SELECT name, path FROM projects WHERE id = ?1", params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;

    let mut stmt = conn.prepare(
        "SELECT agent, COUNT(*), COALESCE(SUM(message_count),0), MAX(ended_at)
         FROM sessions WHERE project_id = ?1 AND deleted_at IS NULL GROUP BY agent",
    )?;
    let agents: Vec<Value> = stmt
        .query_map(params![project_id], |r| {
            Ok(json!({
                "agent": r.get::<_, String>(0)?,
                "sessions": r.get::<_, i64>(1)?,
                "messages": r.get::<_, i64>(2)?,
                "last_activity": r.get::<_, Option<String>>(3)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT id, agent, started_at, ended_at, message_count
         FROM sessions WHERE project_id = ?1 AND deleted_at IS NULL ORDER BY ended_at DESC LIMIT 5",
    )?;
    let recent: Vec<Value> = stmt
        .query_map(params![project_id], |r| {
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "agent": r.get::<_, String>(1)?,
                "started_at": r.get::<_, Option<String>>(2)?,
                "ended_at": r.get::<_, Option<String>>(3)?,
                "messages": r.get::<_, i64>(4)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let handoff = latest_handoff(conn, project_id)?;
    let memories = memories_for_context(conn, project_id)?;
    let artifact_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_artifacts WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;

    Ok(json!({
        "project": name,
        "path": path,
        "by_agent": agents,
        "memories": memories,
        "recent_sessions": recent,
        "latest_handoff": handoff,
        "artifact_count": artifact_count,
        "open_tasks": open_tasks(conn, Some(project_id))?,
    }))
}

// ---------------------------------------------------------------- handoffs

#[derive(Default)]
pub struct HandoffFields<'a> {
    pub title: &'a str,
    pub done: &'a str,
    pub state: &'a str,
    pub decisions: &'a str,
    pub files_changed: &'a str,
    pub open_issues: &'a str,
    pub next_steps: &'a str,
    pub session_id: Option<&'a str>,
}

pub fn create_handoff(conn: &Connection, project_id: i64, f: &HandoffFields<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO handoffs(project_id, session_id, title, done, state, decisions,
                              files_changed, open_issues, next_steps, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            project_id, f.session_id, f.title, f.done, f.state, f.decisions,
            f.files_changed, f.open_issues, f.next_steps, now_iso()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn latest_handoff(conn: &Connection, project_id: i64) -> Result<Option<Value>> {
    let row = conn
        .query_row(
            "SELECT id, session_id, title, done, state, decisions, files_changed,
                    open_issues, next_steps, created_at
             FROM handoffs WHERE project_id = ?1 ORDER BY id DESC LIMIT 1",
            params![project_id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "session_id": r.get::<_, Option<String>>(1)?,
                    "title": r.get::<_, String>(2)?,
                    "done": r.get::<_, String>(3)?,
                    "state": r.get::<_, String>(4)?,
                    "decisions": r.get::<_, String>(5)?,
                    "files_changed": r.get::<_, String>(6)?,
                    "open_issues": r.get::<_, String>(7)?,
                    "next_steps": r.get::<_, String>(8)?,
                    "created_at": r.get::<_, String>(9)?,
                }))
            },
        )
        .optional()?;
    Ok(row)
}

// ---------------------------------------------------------------- sessions

pub fn read_session(
    conn: &Connection,
    session_id: &str,
    max_messages: u32,
    focus_line: Option<i64>,
    before_compact: bool,
) -> Result<Value> {
    let sess = conn
        .query_row(
            "SELECT s.agent, s.cwd, s.started_at, s.ended_at, s.message_count, p.name,
                    s.native_id, s.file_path, s.compact_line_no
             FROM sessions s LEFT JOIN projects p ON p.id = s.project_id WHERE s.id = ?1 AND s.deleted_at IS NULL",
            params![session_id],
            |r| {
                let agent: String = r.get(0)?;
                let cwd: Option<String> = r.get(1)?;
                let native_id: String = r.get(6)?;
                Ok(json!({
                    "agent": agent,
                    "cwd": cwd,
                    "started_at": r.get::<_, Option<String>>(2)?,
                    "ended_at": r.get::<_, Option<String>>(3)?,
                    "message_count": r.get::<_, i64>(4)?,
                    "project": r.get::<_, Option<String>>(5)?,
                    "native_id": native_id,
                    "file_path": r.get::<_, String>(7)?,
                    // 压缩点（0.3.8）：首个携带 harness 摘要的请求行；NULL = 未压缩过
                    "compact_line_no": r.get::<_, Option<i64>>(8)?,
                    // 续聊命令：只展示与复制，不代为执行（DESIGN-0.3 §4）。
                    "resume_command": crate::adapters::resume_command(&agent, &native_id),
                }))
            },
        )
        .optional()?
        .with_context(|| format!("session not found: {session_id}"))?;
    // before_compact（0.3.8）：只取压缩点之前的消息——"压缩前对话备份"切片。
    // 会话无压缩点时 bound 为 None，过滤条件恒真，等于全量（调用方靠
    // compact_line_no 字段区分"没压缩"与"切片为空"）。
    let bound: Option<i64> = if before_compact {
        sess["compact_line_no"].as_i64()
    } else {
        None
    };

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND (?2 IS NULL OR line_no < ?2)",
        params![session_id, bound],
        |r| r.get(0),
    )?;
    let take = max_messages as i64;
    // Long sessions: the current state lives at the tail, not the head, so an
    // excerpt returns the LAST N messages. Short sessions return everything.
    // focus_line（来源指针跳转）：截取"该行及其之前"的尾部窗口，保证目标行
    // 一定在结果里（早于全量尾部窗口的历史行也能定位）。
    let tail_excerpt = total > take;
    let (sql, tail_excerpt) = if focus_line.is_some() {
        (
            "SELECT kind, content, timestamp, line_no FROM messages
             WHERE session_id = ?1 AND line_no <= ?2 AND (?4 IS NULL OR line_no < ?4) ORDER BY line_no DESC, ord DESC LIMIT ?3",
            true,
        )
    } else if tail_excerpt {
        (
            "SELECT kind, content, timestamp, line_no FROM messages
             WHERE session_id = ?1 AND (?3 IS NULL OR line_no < ?3) ORDER BY line_no DESC, ord DESC LIMIT ?2",
            true,
        )
    } else {
        (
            "SELECT kind, content, timestamp, line_no FROM messages
             WHERE session_id = ?1 AND (?3 IS NULL OR line_no < ?3) ORDER BY line_no, ord LIMIT ?2",
            false,
        )
    };
    let mut stmt = conn.prepare(sql)?;
    let mut msgs: Vec<Value> = if focus_line.is_some() {
        stmt.query_map(params![session_id, focus_line, take, bound], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?))
        })?
        .collect::<std::result::Result<Vec<(String, String, Option<String>, i64)>, _>>()?
        .into_iter()
        .map(|(kind, content, timestamp, line_no)| {
            json!({ "kind": kind, "content": content.chars().take(4000).collect::<String>(), "timestamp": timestamp, "line_no": line_no })
        })
        .collect()
    } else {
        stmt.query_map(params![session_id, take, bound], |r| {
            let content: String = r.get(1)?;
            let truncated: String = content.chars().take(4000).collect();
            Ok(json!({
                "kind": r.get::<_, String>(0)?,
                "content": truncated,
                "timestamp": r.get::<_, Option<String>>(2)?,
                "line_no": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if tail_excerpt {
        msgs.reverse();
    }

    let lineage = lineage_for(conn, session_id)?;
    let lineage_tree = lineage_tree(conn, session_id)?;
    Ok(json!({
        "session": sess,
        "lineage": lineage,
        "lineage_tree": lineage_tree,
        "total_messages": total,
        "tail_excerpt": tail_excerpt,
        "before_compact": before_compact,
        "messages": msgs,
    }))
}

pub fn recent_sessions(conn: &Connection, project_id: Option<i64>, limit: u32) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.agent, p.name, s.started_at, s.ended_at, s.message_count,
                (SELECT replace(substr(m.content, 1, 160), char(10), ' ')
                 FROM messages m WHERE m.session_id = s.id AND m.kind = 'user'
                   AND substr(ltrim(m.content), 1, 1) NOT IN ('<', '#')
                 ORDER BY m.line_no, m.ord LIMIT 1)
         FROM sessions s LEFT JOIN projects p ON p.id = s.project_id
         WHERE s.deleted_at IS NULL AND (?1 IS NULL OR s.project_id = ?1)
         ORDER BY s.ended_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project_id, limit as i64], |r| {
        Ok(json!({
            "session_id": r.get::<_, String>(0)?,
            "agent": r.get::<_, String>(1)?,
            "project": r.get::<_, Option<String>>(2)?,
            "started_at": r.get::<_, Option<String>>(3)?,
            "ended_at": r.get::<_, Option<String>>(4)?,
            "messages": r.get::<_, i64>(5)?,
            "preview": r.get::<_, Option<String>>(6)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

// ------------------------------------------------------------- trash (§6)

/// 软删 / 恢复：只翻动 sessions.deleted_at（FTS 行不动，恢复后原样可搜）。
/// 幂等防呆：对已处于目标状态的会话报错。
pub fn set_session_deleted(conn: &Connection, session_id: &str, deleted: bool) -> Result<()> {
    let n = if deleted {
        conn.execute(
            "UPDATE sessions SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![session_id, now_iso()],
        )?
    } else {
        conn.execute(
            "UPDATE sessions SET deleted_at = NULL, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![session_id, now_iso()],
        )?
    };
    let state = if deleted { "not found or already in trash" } else { "not in trash" };
    anyhow::ensure!(n > 0, "session {state}: {session_id}");
    Ok(())
}

mod purge;
pub use purge::{TRASH_RETENTION_DAYS, purge_plan, purge_session, gc_sweep, trash_overdue_plan, purge_trash};

pub fn trash_sessions(conn: &Connection) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.agent, p.name, s.started_at, s.ended_at, s.message_count, s.deleted_at
         FROM sessions s LEFT JOIN projects p ON p.id = s.project_id
         WHERE s.deleted_at IS NOT NULL ORDER BY s.deleted_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let deleted_at: String = r.get(6)?;
        // 超期判定在 Rust 侧用 chrono 解析（RFC3339 带 'T'/毫秒，SQLite datetime
        // 口径不同——S12 教训）。remaining_days 为负表示已超期 |n| 天。
        let (overdue, remaining_days) = match chrono::DateTime::parse_from_rfc3339(&deleted_at) {
            Ok(t) => {
                let age = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_days();
                (age >= TRASH_RETENTION_DAYS, TRASH_RETENTION_DAYS - age)
            }
            Err(_) => (false, TRASH_RETENTION_DAYS),
        };
        Ok(json!({
            "session_id": r.get::<_, String>(0)?,
            "agent": r.get::<_, String>(1)?,
            "project": r.get::<_, Option<String>>(2)?,
            "started_at": r.get::<_, Option<String>>(3)?,
            "ended_at": r.get::<_, Option<String>>(4)?,
            "messages": r.get::<_, i64>(5)?,
            "deleted_at": deleted_at,
            "overdue": overdue,
            "remaining_days": remaining_days,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

// ------------------------------------------------------------------- stats

pub fn stats(conn: &Connection) -> Result<Value> {
    let get = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let mut stmt = conn.prepare(
        "SELECT agent, COUNT(*), COALESCE(SUM(message_count),0) FROM sessions GROUP BY agent",
    )?;
    let agents: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "agent": r.get::<_, String>(0)?,
                "sessions": r.get::<_, i64>(1)?,
                "messages": r.get::<_, i64>(2)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(json!({
        "projects": get("SELECT COUNT(*) FROM projects WHERE archived_at IS NULL"),
        "sessions": get("SELECT COUNT(*) FROM sessions"),
        "messages": get("SELECT COUNT(*) FROM messages"),
        "handoffs": get("SELECT COUNT(*) FROM handoffs"),
        "memories": get("SELECT COUNT(*) FROM memories"),
        "memories_suggested": get("SELECT COUNT(*) FROM memories WHERE status='suggested'"),
        "artifacts": get("SELECT COUNT(*) FROM session_artifacts"),
        "session_links": get("SELECT COUNT(*) FROM session_links"),
        "vault_lines": get("SELECT COUNT(*) FROM vault_lines"),
        "sessions_trash": get("SELECT COUNT(*) FROM sessions WHERE deleted_at IS NOT NULL"),
        "by_agent": agents,
        "usage_last_7d": usage_summary(conn, 7)?,
    }))
}

// ---------------------------------------------------------------- usage log

/// 使用侧指标（DESIGN-0.3 §11）：本地记录命令/工具名 + 时间戳，不外传不上报。
pub fn log_usage(conn: &Connection, source: &str, name: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO usage_log(ts, source, name) VALUES (?1, ?2, ?3)",
        params![now_iso(), source, name],
    )?;
    Ok(())
}

/// 最近 N 天的使用汇总（source × name 分组计数）。
pub fn usage_summary(conn: &Connection, days: u32) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        // ts 是 RFC3339（'T' 分隔）；datetime('now') 出空格分隔——字符串比较时
        // 'T' > ' '，截止日当天的记录全被算进窗口（"7 天"实为 8 天）。
        // strftime 产同格式再比。
        "SELECT source, name, COUNT(*) FROM usage_log
         WHERE ts >= strftime('%Y-%m-%dT%H:%M:%fZ','now',?1) GROUP BY source, name ORDER BY COUNT(*) DESC",
    )?;
    let rows = stmt.query_map(params![format!("-{days} days")], |r| {
        Ok(json!({
            "source": r.get::<_, String>(0)?,
            "name": r.get::<_, String>(1)?,
            "count": r.get::<_, i64>(2)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

// ----------------------------------------------------------------- memory

pub const MEMORY_TYPES: [&str; 7] =
    ["fact", "decision", "rule", "task", "lesson", "preference", "context"];
pub const MEMORY_SCOPES: [&str; 3] = ["global", "project", "session"];
pub const MEMORY_STATUSES: [&str; 4] = ["suggested", "confirmed", "superseded", "archived"];

pub struct MemoryInput<'a> {
    pub project_id: Option<i64>,
    pub scope: &'a str,
    pub r#type: &'a str,
    pub content: &'a str,
    pub status: Option<&'a str>,
    pub source_session_id: Option<&'a str>,
    pub source_message_id: Option<i64>,
}

/// CLI/MCP 共用写入顺序；相似项是提示，不阻止保存，且不包含本次新记录。
pub fn save_memory_with_similar(conn: &Connection, input: &MemoryInput<'_>) -> Result<(String, Vec<Value>)> {
    let similar = find_similar(conn, input.content, input.project_id, input.r#type)?;
    let id = save_memory(conn, input)?;
    Ok((id, similar))
}

/// Save a memory. Decisions and rules default to `suggested` (they shape
/// future agent behavior, so a human should confirm them); everything else
/// defaults to `confirmed`.
pub fn save_memory(conn: &Connection, input: &MemoryInput<'_>) -> Result<String> {
    anyhow::ensure!(MEMORY_TYPES.contains(&input.r#type), "invalid memory type: {}", input.r#type);
    anyhow::ensure!(MEMORY_SCOPES.contains(&input.scope), "invalid memory scope: {}", input.scope);
    if let Some(s) = input.status {
        anyhow::ensure!(MEMORY_STATUSES.contains(&s), "invalid memory status: {s}");
    }
    anyhow::ensure!(!input.content.trim().is_empty(), "memory content is empty");

    let project_id = if input.scope == "global" { None } else { input.project_id };
    anyhow::ensure!(input.scope == "global" || project_id.is_some(),
        "project/session-scoped memory requires a project");
    // 来源指针写入即校验：message 必须属于所指会话，否则行号会错挂（codex 复现）
    if let (Some(sid), Some(mid)) = (input.source_session_id, input.source_message_id) {
        let ok: Option<i64> = conn
            .query_row(
                "SELECT id FROM messages WHERE id = ?1 AND session_id = ?2",
                params![mid, sid],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(ok.is_some(), "source_message_id {mid} 不属于会话 {sid}，拒绝写入");
    }

    let status = input.status.unwrap_or(match input.r#type {
        "rule" | "decision" => "suggested",
        _ => "confirmed",
    });
    let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let now = now_iso();
    conn.execute(
        "INSERT INTO memories(id, project_id, scope, type, content, status,
                              source_session_id, source_message_id, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
        params![id, project_id, input.scope, input.r#type, input.content, status,
                input.source_session_id, input.source_message_id, now],
    )?;
    Ok(id)
}

fn memory_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "project_id": r.get::<_, Option<i64>>(1)?,
        "project": r.get::<_, Option<String>>(2)?,
        "scope": r.get::<_, String>(3)?,
        "type": r.get::<_, String>(4)?,
        "content": r.get::<_, String>(5)?,
        "status": r.get::<_, String>(6)?,
        "source_session_id": r.get::<_, Option<String>>(7)?,
        "source_message_id": r.get::<_, Option<i64>>(8)?,
        "superseded_by": r.get::<_, Option<String>>(9)?,
        "created_at": r.get::<_, String>(10)?,
        "updated_at": r.get::<_, String>(11)?,
        // 来源会话的 agent（无来源指针的记忆为 null——手动 add 的没有 agent 归属）
        "source_agent": r.get::<_, Option<String>>(12)?,
        "source_line_no": r.get::<_, Option<i64>>(13)?,
    }))
}

const MEMORY_SELECT: &str = "
    SELECT m.id, m.project_id, p.name, m.scope, m.type, m.content, m.status,
           m.source_session_id, m.source_message_id, m.superseded_by,
           m.created_at, m.updated_at, s.agent, msg.line_no
    FROM memories m LEFT JOIN projects p ON p.id = m.project_id
    LEFT JOIN sessions s ON s.id = m.source_session_id
    LEFT JOIN messages msg ON msg.id = m.source_message_id AND msg.session_id = s.id AND s.deleted_at IS NULL
";

pub struct MemoryFilter {
    pub project_id: Option<i64>,
    pub scope: Option<String>,
    pub r#type: Option<String>,
    /// Default: suggested + confirmed (the "active" ones).
    pub status: Option<String>,
    /// 按来源会话的 agent 过滤（无来源指针的记忆不属于任何 agent，会被滤掉）。
    pub agent: Option<String>,
    pub include_global: bool,
    pub limit: u32,
}

fn memory_where(f: &MemoryFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = String::from(" WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(pid) = f.project_id {
        if f.include_global {
            sql.push_str(" AND (m.project_id = ? OR m.project_id IS NULL)");
        } else {
            sql.push_str(" AND m.project_id = ?");
        }
        args.push(Box::new(pid));
    }
    if let Some(scope) = &f.scope {
        sql.push_str(" AND m.scope = ?");
        args.push(Box::new(scope.clone()));
    }
    if let Some(t) = &f.r#type {
        sql.push_str(" AND m.type = ?");
        args.push(Box::new(t.clone()));
    }
    if let Some(a) = &f.agent {
        sql.push_str(" AND s.agent = ?");
        args.push(Box::new(a.clone()));
    }
    match &f.status {
        // "all"：不过滤状态——关系图需要跨状态取完整演进链（superseded 在旧
        // 记忆上、confirmed 在链头，任何单状态筛选都断链）
        Some(s) if s == "all" => {}
        Some(s) => {
            sql.push_str(" AND m.status = ?");
            args.push(Box::new(s.clone()));
        }
        None => sql.push_str(" AND m.status IN ('suggested','confirmed')"),
    }
    (sql, args)
}

pub fn list_memories(conn: &Connection, f: &MemoryFilter) -> Result<Vec<Value>> {
    let (w, args) = memory_where(f);
    // limit=0：不设上限（关系图要全链——任何硬上限都会按 updated_at DESC
    // 截掉最旧的链尾，连线静默断裂；codex 三审）
    let sql = match f.limit {
        0 => format!("{MEMORY_SELECT} {w} ORDER BY m.updated_at DESC"),
        n => format!("{MEMORY_SELECT} {w} ORDER BY m.updated_at DESC LIMIT {n}"),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), memory_json)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn search_memory(conn: &Connection, query: &str, f: &MemoryFilter) -> Result<Vec<Value>> {
    let (w, mut args) = memory_where(f);
    let use_fts = query.split_whitespace().all(|t| t.chars().count() >= 3) && !query.trim().is_empty();
    let cond = if use_fts {
        args.insert(0, Box::new(fts_query(query)));
        "m.rowid IN (SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?)".to_string()
    } else {
        // 多词回退保持 AND 语义（与 FTS 一致）；空查询沿用原语义（全命中）
        let tokens: Vec<String> = query.split_whitespace().map(|t| format!("%{}%", like_escape(t))).collect();
        let cond = if tokens.is_empty() {
            "1=1".to_string()
        } else {
            vec!["m.content LIKE ? ESCAPE '\\'"; tokens.len()].join(" AND ")
        };
        for t in tokens.into_iter().rev() {
            args.insert(0, Box::new(t));
        }
        cond
    };
    // limit=0 与 list_memories 同语义（无上限）：search 走 LIMIT 0 会返回空集，
    // 两处口径必须一致（自检 C7；当前调用方都传非零，防御性对齐）
    let sql = match f.limit {
        0 => format!("{MEMORY_SELECT} {} ORDER BY m.updated_at DESC", w.replacen("1=1", &format!("1=1 AND {cond}"), 1)),
        n => format!(
            "{MEMORY_SELECT} {} ORDER BY m.updated_at DESC LIMIT {n}",
            w.replacen("1=1", &format!("1=1 AND {cond}"), 1),
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), memory_json)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// 写路径纪律（吸收自 engramory 的"写前查重、更新而非复制"）：
/// 保存前找疑似重复——同项目（或全局）、同类型、活跃状态的记忆里，
/// 内容 trigram 重合度排序取前 3。确定性启发式，只提醒不拦截（知情门控）。
pub fn find_similar(conn: &Connection, content: &str, project_id: Option<i64>, r#type: &str) -> Result<Vec<Value>> {
    // 采样 trigram：中文无词边界，五点采样（1/4、1/2、3/4 处为主，首尾兜底）
    // 做 FTS 召回——重复内容通常共享主体，纯尾部追加（"（重申）"类）不能
    // 让探针全落空
    let chars: Vec<char> = content.chars().collect();
    if chars.len() < 3 {
        return Ok(Vec::new());
    }
    let tri = |start: usize| chars[start..start + 3].iter().collect::<String>();
    let n = chars.len();
    let probes = [
        tri(0),
        tri(n / 4),
        tri(n / 2),
        tri(3 * n / 4),
        tri(n - 3),
    ];
    let distinct: Vec<String> = {
        let mut v = probes.to_vec();
        v.sort();
        v.dedup();
        v
    };
    // FTS 召回（OR），再按共享 trigram 数排序，阈值 ≥2 视为疑似。
    // type/status/项目过滤下推到 SQL：先 LIMIT 200 后在 Rust 过滤的话，记忆量
    // 大时同项目同类型的真候选可能被其他项目挤掉（codex 评审 suggestion）
    let query = distinct
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut stmt = conn.prepare(&format!(
        "{MEMORY_SELECT} WHERE m.rowid IN (SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1)
           AND m.type = ?2
           AND m.status IN ('suggested','confirmed')
           AND ((?3 IS NULL AND m.project_id IS NULL) OR m.project_id = ?3)
         LIMIT 200"
    ))?;
    let candidates: Vec<Value> = stmt
        .query_map(rusqlite::params![query, r#type, project_id], memory_json)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let shared = |c: &str| -> usize {
        distinct.iter().filter(|t| c.contains(t.as_str())).count()
    };
    let mut hits: Vec<Value> = candidates
        .into_iter()
        .filter(|m| shared(m["content"].as_str().unwrap_or("")) >= 2.min(distinct.len()))
        .map(|mut m| {
            m["shared_trigrams"] = json!(shared(m["content"].as_str().unwrap_or("")));
            m
        })
        .collect();
    hits.sort_by_key(|m| std::cmp::Reverse(m["shared_trigrams"].as_i64().unwrap_or(0)));
    hits.truncate(3);
    Ok(hits)
}

pub fn update_memory_status(conn: &Connection, id: &str, action: &str, superseded_by: Option<&str>) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let new_status = match action {
        "confirm" => "confirmed",
        "archive" => "archived",
        "supersede" => "superseded",
        _ => anyhow::bail!("invalid action: {action} (confirm|archive|supersede)"),
    };
    if action == "supersede" {
        anyhow::ensure!(superseded_by.is_some(), "supersede requires --by <memory_id>");
        let target = superseded_by.unwrap();
        anyhow::ensure!(target != id, "memory cannot supersede itself");
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)", [target], |r| r.get(0))?;
        anyhow::ensure!(exists, "replacement memory not found: {target}");
        let cycle: bool = tx.query_row(
            "WITH RECURSIVE chain(id) AS (
                SELECT ?1 UNION SELECT m.superseded_by FROM memories m JOIN chain c ON m.id = c.id
                WHERE m.superseded_by IS NOT NULL
             ) SELECT EXISTS(SELECT 1 FROM chain WHERE id = ?2)",
            params![target, id], |r| r.get(0),
        )?;
        anyhow::ensure!(!cycle, "replacement would create a memory cycle");
    }
    // confirm 必须清掉 superseded_by：被取代的记忆重新确认后就不再是"被取代"，
    // 保留旧指针会让 A→B、B→A 成环，卷宗演变链遍历会死循环（codex 评审复现）。
    let superseded_by = if action == "supersede" { superseded_by } else { None };
    let n = tx.execute(
        "UPDATE memories SET status = ?1, superseded_by = ?2, updated_at = ?3
         WHERE id = ?4",
        params![new_status, superseded_by, now_iso(), id],
    )?;
    anyhow::ensure!(n > 0, "memory not found: {id}");
    tx.commit()?;
    Ok(())
}

/// Confirmed memories (project + global) and pending-suggestion count,
/// for get_project_context.
pub fn memories_for_context(conn: &Connection, project_id: i64) -> Result<Value> {
    let confirmed = list_memories(conn, &MemoryFilter {
        project_id: Some(project_id),
        scope: None,
        r#type: None,
        status: Some("confirmed".into()),
        agent: None,
        include_global: true,
        limit: 20,
    })?;
    let suggested: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE status = 'suggested' AND (project_id = ?1 OR project_id IS NULL)",
        params![project_id],
        |r| r.get(0),
    )?;
    Ok(json!({ "confirmed": confirmed, "suggested_pending": suggested }))
}

pub fn open_tasks(conn: &Connection, project_id: Option<i64>) -> Result<Vec<Value>> {
    list_memories(conn, &MemoryFilter {
        project_id,
        scope: None,
        r#type: Some("task".into()),
        status: Some("confirmed".into()),
        agent: None,
        include_global: project_id.is_some(),
        limit: 50,
    })
}

// ----------------------------------------------------------- memory files
// Native agent memory file backups (DESIGN-0.3 §2) — separate from the
// curated `memories` table above: these are whole-file snapshots with
// revision history, backed by vault objects.

/// Upsert one monitored file. Returns (file_id, changed): `changed` means the
/// hash differs from the stored one (or the file is new) and a new revision
/// should be recorded.
pub fn upsert_memory_file(conn: &Connection, agent: &str, scope: &str, path: &str, hash: &str) -> Result<(i64, bool)> {
    let existing = conn
        .query_row(
            "SELECT id, current_hash FROM memory_files WHERE agent = ?1 AND path = ?2",
            params![agent, path],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let now = now_iso();
    match existing {
        Some((id, current)) if current == hash => Ok((id, false)),
        Some((id, _)) => {
            conn.execute(
                "UPDATE memory_files SET current_hash = ?1, updated_at = ?2 WHERE id = ?3",
                params![hash, now, id],
            )?;
            Ok((id, true))
        }
        None => {
            conn.execute(
                "INSERT INTO memory_files(agent, scope, path, current_hash, updated_at) VALUES (?1,?2,?3,?4,?5)",
                params![agent, scope, path, hash, now],
            )?;
            Ok((conn.last_insert_rowid(), true))
        }
    }
}

pub fn insert_memory_revision(conn: &Connection, file_id: i64, hash: &str, size: u64) -> Result<i64> {
    conn.execute(
        "INSERT INTO memory_revisions(file_id, hash, size, captured_at) VALUES (?1,?2,?3,?4)",
        params![file_id, hash, size as i64, now_iso()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// memory_fts indexes only the latest revision: delete + reinsert on change.
pub fn set_memory_fts(conn: &Connection, file_id: i64, content: &str) -> Result<()> {
    conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![file_id])?;
    conn.execute("INSERT INTO memory_fts(rowid, content) VALUES (?1, ?2)", params![file_id, content])?;
    Ok(())
}

pub fn list_memory_files(conn: &Connection) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.agent, f.scope, f.path, f.current_hash, f.updated_at,
                COUNT(r.id) AS revisions, MAX(r.captured_at) AS last_captured
         FROM memory_files f LEFT JOIN memory_revisions r ON r.file_id = f.id
         GROUP BY f.id ORDER BY f.agent, f.scope, f.path",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "agent": r.get::<_, String>(1)?,
            "scope": r.get::<_, String>(2)?,
            "path": r.get::<_, String>(3)?,
            "current_hash": r.get::<_, String>(4)?,
            "updated_at": r.get::<_, String>(5)?,
            "revisions": r.get::<_, i64>(6)?,
            "last_captured": r.get::<_, Option<String>>(7)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn memory_file_by_id(conn: &Connection, file_id: i64) -> Result<Option<Value>> {
    let row = list_memory_files(conn)?.into_iter().find(|f| f["id"].as_i64() == Some(file_id));
    Ok(row)
}

/// Revision timeline, newest first.
pub fn memory_file_revisions(conn: &Connection, file_id: i64) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, hash, size, captured_at FROM memory_revisions
         WHERE file_id = ?1 ORDER BY captured_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![file_id], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "hash": r.get::<_, String>(1)?,
            "size": r.get::<_, i64>(2)?,
            "captured_at": r.get::<_, String>(3)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// FTS5 trigram search over latest revisions (callers handle the <3-char
/// fallback — the content lives in the vault, not in this index).
pub fn search_memory_files_fts(conn: &Connection, query: &str, limit: u32) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.agent, f.scope, f.path, f.updated_at
         FROM memory_files f WHERE f.id IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?1)
         ORDER BY f.updated_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![fts_query(query), limit as i64], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "agent": r.get::<_, String>(1)?,
            "scope": r.get::<_, String>(2)?,
            "path": r.get::<_, String>(3)?,
            "updated_at": r.get::<_, String>(4)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

// -------------------------------------------------------------- artifacts

pub fn insert_artifacts(
    conn: &Connection,
    session_id: &str,
    project_id: Option<i64>,
    artifacts: &[crate::models::NewArtifact],
) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO session_artifacts(session_id, project_id, path, tool, created_at)
         VALUES (?1,?2,?3,?4,?5)",
    )?;
    for a in artifacts {
        stmt.execute(params![session_id, project_id, a.path, a.tool, now_iso()])?;
    }
    Ok(())
}

pub fn list_artifacts(conn: &Connection, project_id: Option<i64>, session_id: Option<&str>, limit: u32) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT a.path, a.tool, a.session_id, s.agent, p.name, a.created_at
         FROM session_artifacts a
         JOIN sessions s ON s.id = a.session_id
         LEFT JOIN projects p ON p.id = a.project_id
         WHERE (?1 IS NULL OR a.project_id = ?1) AND (?2 IS NULL OR a.session_id = ?2)
           AND s.deleted_at IS NULL
         ORDER BY a.id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![project_id, session_id, limit as i64], |r| {
        Ok(json!({
            "path": r.get::<_, String>(0)?,
            "tool": r.get::<_, Option<String>>(1)?,
            "session_id": r.get::<_, String>(2)?,
            "agent": r.get::<_, String>(3)?,
            "project": r.get::<_, Option<String>>(4)?,
            "created_at": r.get::<_, String>(5)?,
        }))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------- lineage

/// Rebuild lineage edges from stored uuid signals. Idempotent.
///
/// 裁定（DESIGN-0.3 §8，暂缓至 V1.0 前再评估）：每次 import 末尾**全量重扫**
/// 所有带父指针的会话，而非只处理本轮新增。个人数据量级下成本可忽略
/// （带指针的会话数百量级，INSERT OR IGNORE 天然幂等）；若将来会话数上量，
/// 改为增量——只扫本轮更新过的 session。
pub fn detect_lineage(conn: &Connection) -> Result<u64> {
    let mut sessions_stmt = conn.prepare(
        "SELECT id, first_parent_uuid, compact_leaf_uuid FROM sessions
         WHERE first_parent_uuid IS NOT NULL OR compact_leaf_uuid IS NOT NULL",
    )?;
    let sessions: Vec<(String, Option<String>, Option<String>)> = sessions_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut added = 0u64;
    for (child_id, first_parent, compact_leaf) in sessions {
        let candidates = [
            (compact_leaf, "compact"),
            (first_parent, "fork_or_continuation"),
        ];
        for (uuid, kind) in candidates {
            let Some(uuid) = uuid else { continue };
            // Which session owns this uuid?
            let owner: Option<(String, i64)> = conn
                .query_row(
                    "SELECT session_id, MAX(line_no) FROM session_uuids WHERE uuid = ?1 GROUP BY session_id",
                    params![uuid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((parent_id, _)) = owner else { continue };
            if parent_id == child_id {
                continue;
            }
            let link_type = if kind == "compact" {
                "compact"
            } else {
                // continuation when the parent uuid sits at the parent's tail,
                // fork when it branches from the middle.
                let last_line: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(line_no), 0) FROM session_uuids WHERE session_id = ?1",
                    params![parent_id],
                    |r| r.get(0),
                )?;
                let uuid_line: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(line_no), 0) FROM session_uuids WHERE session_id = ?1 AND uuid = ?2",
                    params![parent_id, uuid],
                    |r| r.get(0),
                )?;
                if last_line - uuid_line <= 1 { "continuation" } else { "fork" }
            };
            added += conn.execute(
                "INSERT OR IGNORE INTO session_links(child_session_id, parent_session_id, link_type, via_uuid, created_at)
                 VALUES (?1,?2,?3,?4,?5)",
                params![child_id, parent_id, link_type, uuid, now_iso()],
            )? as u64;
        }
    }
    Ok(added)
}

pub fn lineage_for(conn: &Connection, session_id: &str) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT parent_session_id, link_type FROM session_links WHERE child_session_id = ?1 ORDER BY parent_session_id, link_type",
    )?;
    let parents: Vec<Value> = stmt
        .query_map(params![session_id], |r| {
            Ok(json!({ "session_id": r.get::<_, String>(0)?, "type": r.get::<_, String>(1)? }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT child_session_id, link_type FROM session_links WHERE parent_session_id = ?1 ORDER BY child_session_id, link_type",
    )?;
    let children: Vec<Value> = stmt
        .query_map(params![session_id], |r| {
            Ok(json!({ "session_id": r.get::<_, String>(0)?, "type": r.get::<_, String>(1)? }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(json!({ "parents": parents, "children": children }))
}

/// Transitive lineage family of one session: BFS up parents and down children
/// over session_links (visited set kills cycles, 300-node cap). Sessions missing
/// from the table (purged) come back as ext nodes; trashed ones flagged deleted —
/// both render non-navigable in the UI graph.
pub fn lineage_tree(conn: &Connection, session_id: &str) -> Result<Value> {
    const MAX_NODES: usize = 300;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut edge_set: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut edges: Vec<Value> = Vec::new();
    seen.insert(session_id.to_string());
    let mut queue = std::collections::VecDeque::from([session_id.to_string()]);
    let mut truncated = false;
    let mut q_parents = conn.prepare(
        "SELECT parent_session_id, link_type FROM session_links WHERE child_session_id = ?1 ORDER BY parent_session_id, link_type",
    )?;
    let mut q_children = conn.prepare(
        "SELECT child_session_id, link_type FROM session_links WHERE parent_session_id = ?1 ORDER BY child_session_id, link_type",
    )?;
    while let Some(sid) = queue.pop_front() {
        let parents: Vec<(String, String)> = q_parents
            .query_map(params![sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (p, lt) in parents {
            if !seen.contains(&p) && seen.len() >= MAX_NODES { truncated = true; continue; }
            if edge_set.insert((p.clone(), sid.clone())) {
                edges.push(json!({ "parent": p.clone(), "child": sid.clone(), "link_type": lt }));
            }
            if seen.insert(p.clone()) {
                queue.push_back(p);
            }
        }
        let children: Vec<(String, String)> = q_children
            .query_map(params![sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (c, lt) in children {
            if !seen.contains(&c) && seen.len() >= MAX_NODES { truncated = true; continue; }
            if edge_set.insert((sid.clone(), c.clone())) {
                edges.push(json!({ "parent": sid.clone(), "child": c.clone(), "link_type": lt }));
            }
            if seen.insert(c.clone()) {
                queue.push_back(c);
            }
        }
    }
    let mut q_meta = conn.prepare(
        "SELECT s.agent, s.started_at, s.message_count, p.name, s.deleted_at,
                (SELECT replace(substr(m.content, 1, 80), char(10), ' ') FROM messages m
                 WHERE m.session_id = s.id AND m.kind = 'user'
                   AND substr(ltrim(m.content), 1, 1) NOT IN ('<', '#')
                 ORDER BY m.line_no, m.ord LIMIT 1),
                (SELECT replace(substr(m.content, 1, 80), char(10), ' ') FROM messages m
                 WHERE m.session_id = s.id AND m.kind = 'assistant'
                 ORDER BY m.line_no DESC, m.ord DESC LIMIT 1)
         FROM sessions s LEFT JOIN projects p ON p.id = s.project_id WHERE s.id = ?1",
    )?;
    let mut nodes: Vec<Value> = Vec::new();
    for sid in &seen {
        let row = q_meta
            .query_row(params![sid], |r| {
                Ok(json!({
                    "session_id": sid,
                    "agent": r.get::<_, String>(0)?,
                    "started_at": r.get::<_, Option<String>>(1)?,
                    "messages": r.get::<_, i64>(2)?,
                    "project": r.get::<_, Option<String>>(3)?,
                    "deleted": r.get::<_, Option<String>>(4)?.is_some(),
                    // 无 LLM 摘要口径（0.4.2 用户反馈"要看进度树"）：title=这段对话
                    // 要干什么（首条用户消息，同列表页摘要），tail=停在哪（末条 assistant）
                    "title": r.get::<_, Option<String>>(5)?,
                    "tail": r.get::<_, Option<String>>(6)?,
                }))
            })
            .optional()?;
        nodes.push(row.unwrap_or_else(|| json!({ "session_id": sid, "ext": true })));
    }
    nodes.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
    Ok(json!({ "nodes": nodes, "edges": edges, "truncated": truncated, "node_limit": MAX_NODES }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_session(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO sessions (id, agent, native_id, file_path, created_at, updated_at)
             VALUES (?1, 'claude', ?2, '/tmp/x.jsonl', '2026-09-05T00:00:00Z', '2026-09-05T00:00:00Z')",
            params![id, id],
        )
        .unwrap();
    }

    fn seed_message(conn: &Connection, session: &str, kind: &str, content: &str) {
        conn.execute(
            "INSERT INTO messages (session_id, line_no, kind, content)
             VALUES (?1, (SELECT COALESCE(MAX(line_no), 0) + 1 FROM messages WHERE session_id = ?1), ?2, ?3)",
            params![session, kind, content],
        )
        .unwrap();
    }

    fn search_hits(conn: &Connection, query: &str) -> usize {
        search(
            conn,
            &SearchOpts {
                query: query.into(),
                project: None,
                agent: None,
                kind: None,
                limit: 50,
            },
        )
        .unwrap()
        .len()
    }

    #[test]
    fn tool_output_not_indexed_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        seed_session(&conn, "claude:t1");
        seed_message(&conn, "claude:t1", "user", "讨论 QUICKUSER 方案");
        seed_message(&conn, "claude:t1", "tool_call", "grep QUICKCALL src/main.rs");
        seed_message(&conn, "claude:t1", "tool_result", "error QUICKRESULT QRMARK boom");
        assert!(!tool_index_enabled(&conn).unwrap(), "默认轻量态");
        assert_eq!(search_hits(&conn, "QUICKUSER"), 1, "对话层可搜");
        assert_eq!(search_hits(&conn, "QUICKCALL"), 1, "tool_call 保留在对话层");
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 0, "轻量态搜不到工具输出");
        assert_eq!(search_hits(&conn, "QR"), 1, "短词 LIKE 回退仍可达工具输出");
        let st = index_status(&conn).unwrap();
        assert_eq!(st["indexed_tool_rows"], 0);
        assert_eq!(st["tool_index_full"], false);
    }

    #[test]
    fn tool_index_enable_disable_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let conn = open(&home).unwrap();
        seed_session(&conn, "claude:t2");
        seed_message(&conn, "claude:t2", "tool_result", "panic QUICKRESULT at runtime");

        set_tool_index(&home, &conn, true).unwrap();
        assert!(tool_index_enabled(&conn).unwrap());
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 1, "全量态回填可搜");
        seed_message(&conn, "claude:t2", "tool_result", "second QUICKSECOND output");
        assert_eq!(search_hits(&conn, "QUICKSECOND"), 1, "全量态新写入也被触发器索引");
        assert_eq!(crate::ingest::read_config(&home)[CFG_TOOL_INDEX], json!(true));

        set_tool_index(&home, &conn, false).unwrap();
        assert!(!tool_index_enabled(&conn).unwrap());
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 0, "切回轻量后不可搜");
        seed_message(&conn, "claude:t2", "tool_result", "third QUICKTHIRD output");
        assert_eq!(search_hits(&conn, "QUICKTHIRD"), 0, "轻量态新写入不进索引");
        assert_eq!(crate::ingest::read_config(&home)[CFG_TOOL_INDEX], json!(false));
    }

    #[test]
    fn v11_full_index_db_migrates_to_light() {
        let dir = tempfile::tempdir().unwrap();
        // 按当前代码建库后伪装成 v11 形态：无 WHEN 触发器 + 全量单表索引
        {
            let conn = open(dir.path()).unwrap();
            seed_session(&conn, "claude:t3");
            seed_message(&conn, "claude:t3", "user", "普通对话 QUICKUSER");
            seed_message(&conn, "claude:t3", "tool_result", "error QUICKRESULT boom");
            conn.execute_batch(
                "DROP TRIGGER messages_ai;
                 DROP TRIGGER messages_ad;
                 INSERT INTO messages_fts(messages_fts) VALUES('delete-all');
                 INSERT INTO messages_fts(rowid, content) SELECT id, content FROM messages;
                 CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                   INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content); END;
                 CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
                   INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content); END;
                 PRAGMA user_version = 11;",
            )
            .unwrap();
        }
        let conn = open(dir.path()).unwrap(); // 走 v12 迁移
        assert!(!tool_index_enabled(&conn).unwrap());
        assert_eq!(search_hits(&conn, "QUICKUSER"), 1, "对话层迁移后保留");
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 0, "工具输出迁移后移出对话层索引");
        let r = crate::doctor::run(&conn, dir.path()).unwrap();
        let fts = r["checks"].as_array().unwrap().iter().find(|c| c["name"] == "fts").unwrap();
        assert_eq!(fts["status"], "ok", "doctor 两态对账: {fts}");
    }

    #[test]
    fn open_converges_light_when_config_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        {
            let conn = open(&home).unwrap();
            seed_session(&conn, "claude:t4");
            seed_message(&conn, "claude:t4", "tool_result", "QUICKRESULT one");
            // 手工造成"库里全量、config 缺省轻量"的漂移（enable 后 config 写回失败的窗口）
            enable_tool_index(&conn).unwrap();
            assert!(tool_index_enabled(&conn).unwrap());
        }
        let conn = open(&home).unwrap();
        assert!(!tool_index_enabled(&conn).unwrap(), "open 收敛到 config 轻量态");
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 0);
    }

    #[test]
    fn full_mode_delete_reinsert_stays_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let conn = open(&home).unwrap();
        seed_session(&conn, "claude:t5");
        seed_message(&conn, "claude:t5", "tool_result", "first QUICKRESULT output");
        set_tool_index(&home, &conn, true).unwrap();
        // resync 语义：先清后重插，索引两侧都要跟住
        conn.execute("DELETE FROM messages WHERE content LIKE '%QUICKRESULT%'", [])
            .unwrap();
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 0, "删除后索引同步清空");
        seed_message(&conn, "claude:t5", "tool_result", "replayed QUICKRESULT output");
        assert_eq!(search_hits(&conn, "QUICKRESULT"), 1, "重插后索引同步恢复");
    }
}
