//! doctor：本地自检（UI-DESIGN §8.5，0.4.2）。
//! 全程只读，不改任何数据——它是 vault"可证明"承诺的对账端：
//! 定期抽验比出事时再发现便宜得多。

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::{db, vault};

/// 哈希抽验样本量：全量对账走 `backup export`/`bundle verify`（逐对象），
/// doctor 的定位是快速抽查——固定上限保证大数据集下仍然秒回。
const SAMPLE_LIMIT: i64 = 256;

fn check(name: &str, status: &str, detail: impl Into<String>) -> Value {
    json!({ "name": name, "status": status, "detail": detail.into() })
}

pub fn run(conn: &Connection, home: &Path) -> Result<Value> {
    let mut checks: Vec<Value> = Vec::new();

    // 1. schema 版本与代码期望一致（迁移漏跑会在这里显形）
    let schema: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    checks.push(if schema == db::SCHEMA_VERSION {
        check("schema", "ok", format!("v{schema}（与代码期望一致）"))
    } else {
        check("schema", "fail", format!("数据库 schema 为 v{schema}，代码要求 v{}", db::SCHEMA_VERSION))
    });

    // 2. FTS 与 messages 行数一致（不一致 = 搜索结果不可信）。
    // 轻量索引（1.0.1）分两张表对账：对话层 = 全部 - tool_result；全量态下
    // 工具表还要与 tool_result 行数对齐，轻量态下必须为空。
    let msgs: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let tool_msgs: i64 =
        conn.query_row("SELECT COUNT(*) FROM messages WHERE kind = 'tool_result'", [], |r| r.get(0))?;
    // external content 的 FTS5 表 COUNT(*) 会穿透到内容表（恒等于 messages 行数，
    // 旧对账因此一直测不准）——已索引行数必须数 docsize 影子表。
    let fts: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts_docsize", [], |r| r.get(0))?;
    let tools_fts: i64 = conn.query_row("SELECT COUNT(*) FROM messages_tools_fts_docsize", [], |r| r.get(0))?;
    let full = db::tool_index_enabled(conn)?;
    let (fts_status, fts_detail) = if fts != msgs - tool_msgs {
        ("fail", format!("对话层索引 {fts} 行 vs messages {msgs} 行（工具输出 {tool_msgs} 行）——索引脱节"))
    } else if !full && tools_fts != 0 {
        ("fail", format!("轻量模式下工具索引仍有 {tools_fts} 行——未清干净"))
    } else if full && tools_fts != tool_msgs {
        ("fail", format!("全量模式下工具索引 {tools_fts} 行 vs 工具输出 {tool_msgs} 行——索引脱节"))
    } else if full {
        ("ok", format!("messages {msgs} 行（工具输出 {tool_msgs}），对话层 {fts} + 工具层 {tools_fts}，一致"))
    } else {
        ("ok", format!("messages {msgs} 行，对话层索引 {fts} 行（工具输出 {tool_msgs} 行未索引），一致"))
    };
    checks.push(check("fts", fts_status, fts_detail));

    // 2.5 空闲页占比：删除重导（resync/purge/索引切换）会在库里留洞，
    // VACUUM 可回收。只读报告不自动清——与 GC 同纪律，清理不自动跑。
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
    let page_count: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let freelist: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let ratio = if page_count > 0 { freelist as f64 / page_count as f64 } else { 0.0 };
    checks.push(if ratio < 0.1 {
        check("db_free", "ok", format!("空闲页占比 {:.0}%，无需整理", ratio * 100.0))
    } else {
        check(
            "db_free",
            "warn",
            format!(
                "空闲页占比 {:.0}%（约 {} MB 可回收）——CLI `yourmem index compact` 或设置页「回收空闲空间」可整理",
                ratio * 100.0,
                freelist * page_size / 1_000_000
            ),
        )
    });

    // 3. vault 抽验：随机抽 SAMPLE_LIMIT 个被引用对象重算哈希
    let sample: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT hash FROM (
                SELECT DISTINCT hash FROM vault_lines
                UNION SELECT hash FROM memory_revisions
             ) ORDER BY RANDOM() LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![SAMPLE_LIMIT], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut bad = 0u64;
    for h in &sample {
        if vault::read_object(home, h, true).is_err() {
            bad += 1;
        }
    }
    checks.push(if bad == 0 {
        check("vault_sample", "ok", format!("抽验 {} 个对象，均与归档一致", sample.len()))
    } else {
        check("vault_sample", "fail", format!("抽验 {} 个对象，{} 个与归档不符或缺失", sample.len(), bad))
    });

    // 4. 被引用对象磁盘缺失全量扫描（stat 级别，不重算哈希）
    let referenced: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT hash FROM vault_lines UNION SELECT hash FROM memory_revisions",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let missing = referenced
        .iter()
        .filter(|h| !vault::object_path(home, h).is_file())
        .count();
    checks.push(if missing == 0 {
        check("vault_missing", "ok", format!("{} 个引用对象全部在盘", referenced.len()))
    } else {
        check("vault_missing", "fail", format!("{missing} / {} 个引用对象磁盘缺失，备份不完整", referenced.len()))
    });

    // 5. 盘上未被引用的对象（泄漏：既不属 vault_lines 也不属 memory_revisions）
    let refset: std::collections::HashSet<&str> =
        referenced.iter().map(String::as_str).collect();
    let mut orphans = 0u64;
    let mut tmp_residue = 0u64;
    let root = vault::objects_root(home);
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".tmp.") {
                tmp_residue += 1;
            } else if !refset.contains(name.as_str()) {
                orphans += 1;
            }
        }
    }
    checks.push(if tmp_residue == 0 {
        check("tmp_residue", "ok", "无崩溃残留的 tmp 文件")
    } else {
        check("tmp_residue", "warn", format!("{tmp_residue} 个 .tmp 残留（可手动删除，不影响完整性）"))
    });
    checks.push(if orphans == 0 {
        check("orphan_objects", "ok", "磁盘无未被引用的对象")
    } else {
        check("orphan_objects", "warn", format!("{orphans} 个对象无引用（GC 之外的泄漏；purge 归档在 backups/ 不计入）"))
    });

    // 6. memory_files 当前修订的对象都在（current_hash 悬空 = 详情页打不开）
    let cur_missing = {
        let mut stmt =
            conn.prepare("SELECT current_hash FROM memory_files WHERE current_hash IS NOT NULL")?;
        let hashes: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let m = hashes.iter().filter(|h| !vault::object_path(home, h).is_file()).count();
        m
    };
    checks.push(if cur_missing == 0 {
        check("memory_files", "ok", "memory 文件当前修订对象齐全")
    } else {
        check("memory_files", "fail", format!("{cur_missing} 个 memory 文件的当前修订对象缺失"))
    });

    // 7. 独立数据库快照新鲜度；bundle 可存到任意位置，不在此项检查范围内。
    let snapshots = vault::list_snapshots(home)?;
    let detail = match snapshots.last() {
        None => ("warn", "未发现独立数据库快照，可用 backup db 创建。此项不检查 .tar.gz 完整备份；完整备份请在设置的备份页校验。".to_string()),
        Some(p) => {
            let age = std::fs::metadata(p)
                .and_then(|m| m.modified())
                .map(|t| std::time::SystemTime::now().duration_since(t).unwrap_or_default())
                .unwrap_or_default();
            let days = age.as_secs() / 86400;
            if days > 7 {
                ("warn", format!("最近快照 {days} 天前（共 {} 份）", snapshots.len()))
            } else {
                ("ok", format!("最近快照 {days} 天前（共 {} 份）", snapshots.len()))
            }
        }
    };
    checks.push(check("db_snapshot", detail.0, detail.1));

    let ok = checks.iter().all(|c| c["status"] != "fail");
    Ok(json!({
        "ok": ok,
        "checked_at": crate::now_iso(),
        "checks": checks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_on_fresh_home() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(dir.path()).unwrap();
        let r = run(&conn, dir.path()).unwrap();
        // 全新库：schema/fts/vault 全 ok；快照从未做过 → warn，但整体无 fail
        assert_eq!(r["ok"], true);
        let checks = r["checks"].as_array().unwrap();
        assert!(checks.iter().any(|c| c["name"] == "db_snapshot" && c["status"] == "warn"));
        assert!(checks.iter().all(|c| c["status"] != "fail"));
    }

    #[test]
    fn doctor_catches_missing_object() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(dir.path()).unwrap();
        // 手工插一条引用了不存在对象的 vault 行
        let fake = "ab".repeat(32);
        conn.execute(
            "INSERT INTO vault_lines (session_id, line_no, hash) VALUES ('a:x', 1, ?1)",
            rusqlite::params![fake],
        )
        .unwrap();
        let r = run(&conn, dir.path()).unwrap();
        assert_eq!(r["ok"], false);
        assert!(r["checks"].as_array().unwrap().iter()
            .any(|c| c["name"] == "vault_missing" && c["status"] == "fail"));
    }
}
