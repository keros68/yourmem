//! t7：--purge + 引用计数 GC——共享对象不误删、FTS 联动、保留期门控、幂等重入。

use yourmem::{adapters, db, ingest};

#[test]
fn cli_purge_preserves_json_contract_and_archive_flags() {
    for keep in [false, true] {
        let (home, _, _, c) = setup();
        let conn = db::open(home.path()).unwrap();
        age_out(&conn, &c);
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_yourmem"));
        cmd.args(["session", "purge", &c, "--yes"]).env("YOUMEM_HOME", home.path());
        if keep { cmd.arg("--keep-archive"); }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let values: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&out.stdout)
            .into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["can_purge"], true);
        assert_eq!(values[1]["purged"], c);
        assert_eq!(std::path::Path::new(values[1]["backup_dir"].as_str().unwrap()).exists(), keep);
    }
}

#[test]
fn cli_confirmation_holds_lock_and_cancel_preserves_session() {
    let (home, a, _, _) = setup();
    let mut conn = db::open(home.path()).unwrap();
    age_out(&conn, &a);
    let r = yourmem::trash::purge_cli(&mut conn, home.path(), &a, false, false, |plan| {
        assert_eq!(plan["can_purge"], true);
        assert!(ingest::ImportLockTx::acquire(home.path(), std::time::Duration::ZERO).is_err());
        anyhow::bail!("cancelled")
    });
    assert!(r.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(db::trash_sessions(&conn).unwrap().len(), 1);
}

#[test]
fn selected_purge_continues_after_failure_and_drops_archives() {
    let (home, a, _, c) = setup();
    let mut conn = db::open(home.path()).unwrap();
    age_out(&conn, &a);
    db::set_session_deleted(&conn, &c, true).unwrap();
    let r = yourmem::trash::purge_selected(&mut conn, home.path(), &[c.clone(), a.clone()], false).unwrap();
    assert_eq!(r["purged"], 1);
    assert_eq!(r["failed"][0]["id"], c);
    assert_eq!(db::trash_sessions(&conn).unwrap().len(), 1);
    let r = yourmem::trash::purge_desktop(&mut conn, home.path(), &c, true).unwrap();
    assert!(!std::path::Path::new(r["backup_dir"].as_str().unwrap()).exists());
}

#[test]
fn overdue_execution_uses_previewed_ids_and_keeps_archives() {
    let (home, a, b, _) = setup();
    let mut conn = db::open(home.path()).unwrap();
    age_out(&conn, &a);
    db::set_session_deleted(&conn, &b, true).unwrap();
    let r = yourmem::trash::empty_cli(&mut conn, home.path(), |plan| {
        assert_eq!(plan["overdue_ids"].as_array().unwrap().len(), 1);
        // A second connection simulates another item crossing the cutoff after preview.
        let other = db::open(home.path()).unwrap();
        age_out(&other, &b);
        Ok(())
    }).unwrap();
    assert_eq!(r["purged_sessions"], 1);
    assert!(std::path::Path::new(r["results"][0]["backup_dir"].as_str().unwrap()).exists());
    assert_eq!(db::trash_sessions(&conn).unwrap()[0]["session_id"], b);
    let r = yourmem::trash::empty_desktop(&mut conn, home.path()).unwrap();
    assert_eq!(r["purged_sessions"], 1);
    assert!(std::path::Path::new(r["results"][0]["backup_dir"].as_str().unwrap()).exists());
    assert_eq!(yourmem::trash::empty_on_startup(&mut conn, home.path()).unwrap()["purged_sessions"], 0);
}

const SHARED_LINE: &str = r#"{"type":"user","cwd":"/tmp/gc","uuid":"s1","timestamp":"2026-08-23T09:00:00Z","message":{"role":"user","content":"两个会话完全相同的一行（跨会话共享对象）"}}"#;

fn setup() -> (tempfile::TempDir, String, String, String) {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dir = src.path().join("c");
    std::fs::create_dir_all(&dir).unwrap();
    // 两个会话共享同一行内容 → 同一 CAS hash；第三个会话独占一行
    std::fs::write(dir.join("aaaa.jsonl"), format!("{SHARED_LINE}\n")).unwrap();
    std::fs::write(dir.join("bbbb.jsonl"), format!("{SHARED_LINE}\n")).unwrap();
    std::fs::write(
        dir.join("cccc.jsonl"),
        r#"{"type":"user","cwd":"/tmp/gc","uuid":"u3","timestamp":"2026-08-23T09:00:00Z","message":{"role":"user","content":"独占行内容"}}"#.to_string() + "\n",
    )
    .unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, dir)];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    (home, "claude:aaaa".into(), "claude:bbbb".into(), "claude:cccc".into())
}

/// 把会话的 deleted_at 拨回 31 天前（绕过保留期用于测试）。
fn age_out(conn: &rusqlite::Connection, sid: &str) {
    let old = (chrono::Utc::now() - chrono::Duration::days(31))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    conn.execute(
        "UPDATE sessions SET deleted_at = ?1 WHERE id = ?2",
        rusqlite::params![old, sid],
    )
    .unwrap();
}

fn object_exists(home: &std::path::Path, conn: &rusqlite::Connection, sid: &str, needle: &str) -> bool {
    let Ok(hash) = conn
        .query_row(
            "SELECT hash FROM vault_lines v JOIN messages m ON m.session_id = v.session_id AND m.line_no = v.line_no
             WHERE v.session_id = ?1 AND m.content LIKE ?2 LIMIT 1",
            rusqlite::params![sid, format!("%{needle}%")],
            |r| r.get::<_, String>(0),
        )
    else {
        return false; // 行都没了：对象自然无从谈起
    };
    yourmem::vault::object_path(home, &hash).is_file()
}

#[test]
fn gc_removes_exclusive_keeps_shared_objects() {
    let (home, a, b, c) = setup();
    let mut conn = db::open(home.path()).unwrap();

    // 软删 + 拨过期
    for sid in [&a, &c] {
        db::set_session_deleted(&conn, sid, true).unwrap();
        age_out(&conn, sid);
    }

    // 预览：a 的独占对象是 0（唯一行与 b 共享）；c 的独占对象是 1
    let pa = db::purge_plan(&conn, home.path(), &a).unwrap();
    assert_eq!(pa["can_purge"], true);
    assert_eq!(pa["exclusive_objects"], 0, "a 的唯一行与 b 共享");
    let pc = db::purge_plan(&conn, home.path(), &c).unwrap();
    assert_eq!(pc["exclusive_objects"], 1);

    // 物理清 a：共享对象必须保留
    let r = db::purge_session(&mut conn, home.path(), &a, false, false).unwrap();
    assert_eq!(r["archived_objects"], 0);
    assert_eq!(r["kept_shared_objects"], 1);
    assert!(object_exists(home.path(), &conn, &b, "共享对象"), "b 还引用着，对象不得删");
    // a 的行没了，FTS 对 a 的内容仍能搜到（b 还有同文行）——搜"共享"应命中 b
    let hits = db::search(&conn, &db::SearchOpts {
        query: "跨会话共享对象".into(), project: None, agent: None, kind: None, limit: 10,
    }).unwrap();
    assert_eq!(hits.len(), 1, "只剩 b 的命中: {hits:?}");
    // a 的独有内容（本测试里 a 无独有内容，用会话消失验证）
    let gone: i64 = conn.query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", rusqlite::params![a], |r| r.get(0)).unwrap();
    assert_eq!(gone, 0);

    // 物理清 c：独占对象删除（清除前先取 hash——清除后无从按行查）
    let c_hash: String = conn
        .query_row(
            "SELECT hash FROM vault_lines v JOIN messages m ON m.session_id = v.session_id AND m.line_no = v.line_no
             WHERE v.session_id = ?1 AND m.content LIKE '%独占行%' LIMIT 1",
            rusqlite::params![c],
            |r| r.get(0),
        )
        .unwrap();
    let r = db::purge_session(&mut conn, home.path(), &c, false, false).unwrap();
    assert_eq!(r["archived_objects"], 1);
    assert!(!yourmem::vault::object_path(home.path(), &c_hash).is_file(), "c 清除后独占对象归档（离开主库）");
    // FTS：独占内容再也搜不到
    let hits = db::search(&conn, &db::SearchOpts {
        query: "独占行内容".into(), project: None, agent: None, kind: None, limit: 10,
    }).unwrap();
    assert_eq!(hits.len(), 0, "FTS 联动清理");

    // 再清 b：最后的引用消失，共享对象此时回收
    db::set_session_deleted(&conn, &b, true).unwrap();
    age_out(&conn, &b);
    let r = db::purge_session(&mut conn, home.path(), &b, false, false).unwrap();
    assert_eq!(r["archived_objects"], 1, "最后引用消失后共享对象归档");
}

#[test]
fn retention_gate_and_memory_revision_protection() {
    let (home, a, _b, _c) = setup();
    let mut conn = db::open(home.path()).unwrap();

    // 不在回收站：拒绝
    let p = db::purge_plan(&conn, home.path(), &a).unwrap();
    assert_eq!(p["in_trash"], false);
    assert!(db::purge_session(&mut conn, home.path(), &a, false, false).is_err(), "未软删不得清");

    // 软删但在保留期内：拒绝并给出剩余天数
    db::set_session_deleted(&conn, &a, true).unwrap();
    let p = db::purge_plan(&conn, home.path(), &a).unwrap();
    assert_eq!(p["can_purge"], false);
    assert!(p["remaining_days"].as_i64().unwrap() > 0);
    let err = db::purge_session(&mut conn, home.path(), &a, false, false).unwrap_err().to_string();
    assert!(err.contains("保留期"), "{err}");

    // memory_revisions 引用保护：把 a 的共享 hash 也记成 memory 修订，
    // 即使 b 也被清，对象仍不得删
    age_out(&conn, &a);
    let hash: String = conn
        .query_row("SELECT hash FROM vault_lines WHERE session_id = ?1 LIMIT 1", rusqlite::params![a], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO memory_files(agent, scope, path, current_hash, updated_at)
         VALUES ('claude', 'global', '/tmp/gc/MEMORY.md', ?1, '2026-08-23T00:00:00Z')",
        rusqlite::params![hash],
    )
    .unwrap();
    let fid: i64 = conn.query_row("SELECT id FROM memory_files LIMIT 1", [], |r| r.get(0)).unwrap();
    conn.execute(
        "INSERT INTO memory_revisions(file_id, hash, size, captured_at) VALUES (?1, ?2, 10, '2026-08-23T00:00:00Z')",
        rusqlite::params![fid, hash],
    )
    .unwrap();
    let r = db::purge_session(&mut conn, home.path(), &a, false, false).unwrap();
    // a 清了，但共享行+memory 修订双引用：对象保留
    assert!(yourmem::vault::object_path(home.path(), &hash).is_file(), "memory_revisions 引用的对象不得归档");
    assert_eq!(r["kept_shared_objects"], 1);
}

#[test]
fn purge_trash_only_overdue_and_idempotent() {
    let (home, a, b, c) = setup();
    let mut conn = db::open(home.path()).unwrap();
    // a 过期，b 刚软删（保留期内）
    db::set_session_deleted(&conn, &a, true).unwrap();
    age_out(&conn, &a);
    db::set_session_deleted(&conn, &b, true).unwrap();
    // 集合固定语义：用 plan 的 ids 执行（确认什么就清什么）
    let plan = db::trash_overdue_plan(&conn, home.path()).unwrap();
    let ids: Vec<String> = plan["overdue_ids"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    assert_eq!(ids.len(), 1, "只有超期项进集合");
    let r = db::purge_trash(&mut conn, home.path(), &ids).unwrap();
    assert_eq!(r["purged_sessions"], 1, "只有超期项被清");
    let b_alive: i64 = conn.query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", rusqlite::params![b], |r| r.get(0)).unwrap();
    assert_eq!(b_alive, 1, "保留期内的 b 不动");
    // 幂等重入：再跑一遍，无超期项即 0
    let plan = db::trash_overdue_plan(&conn, home.path()).unwrap();
    let ids: Vec<String> = plan["overdue_ids"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let r = db::purge_trash(&mut conn, home.path(), &ids).unwrap();
    assert_eq!(r["purged_sessions"], 0);
    let _ = c;
}

#[test]
fn tombstone_prevents_revival_and_sweep_reenters() {
    let (home, a, _b, _c) = setup();
    let mut conn = db::open(home.path()).unwrap();
    db::set_session_deleted(&conn, &a, true).unwrap();
    age_out(&conn, &a);

    // purge 后备份产物存在（rows.json + objects/）
    let r = db::purge_session(&mut conn, home.path(), &a, false, false).unwrap();
    let backup = std::path::PathBuf::from(r["backup_dir"].as_str().unwrap());
    assert!(backup.join("rows.json").is_file(), "库行备份必须落盘");
    let rows: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(backup.join("rows.json")).unwrap()).unwrap();
    assert!(rows["messages"].as_array().unwrap().len() >= 1, "备份含被删消息");
    // 终审 blocker：谱系边与导入游标也必须在备份里（恢复原料完整性）
    assert!(rows["session_links"].as_array().is_some(), "谱系边表在备份");
    assert!(rows["source_file"].as_array().is_some(), "导入游标在备份");
    assert!(backup.join("objects").join(r["archived_objects"].as_u64().unwrap_or(0).to_string()).exists()
        || r["archived_objects"].as_u64() == Some(0)
        || std::fs::read_dir(backup.join("objects")).map(|d| d.count() > 0).unwrap_or(false),
        "归档目录非空（若对象被归档）");

    // 墓碑：同一文件再 import 不得复活（codex 七审：游标清了会完整重导）
    let src_root = home.path();
    // 重新导入同一批文件：a 不复活（墓碑），其余照常
    let roots = vec![(adapters::AGENT_CLAUDE, {
        // 从 purged_sources 反查源路径的父目录
        let purged_path: String = conn.query_row(
            "SELECT path FROM purged_sources LIMIT 1", [], |r| r.get(0)).unwrap();
        std::path::PathBuf::from(purged_path).parent().unwrap().to_path_buf()
    })];
    drop(conn);
    let mut conn = db::open(home.path()).unwrap();
    let out = ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    let revived: i64 = conn.query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", rusqlite::params![a], |r| r.get(0)).unwrap();
    assert_eq!(revived, 0, "墓碑必须阻止复活");
    assert_eq!(out.messages_added, 0, "a 的消息不得重导");

    // sweep 重入：伪造中断（直接往 gc_pending 塞一个无引用 hash），sweep 清掉
    conn.execute(
        "INSERT OR IGNORE INTO gc_pending(hash, added_at) VALUES ('deadbeef', '2026-08-23T00:00:00Z')",
        [],
    ).unwrap();
    let sw = db::gc_sweep(&conn, home.path()).unwrap();
    assert_eq!(sw["swept"], 0, "不存在的对象直接出队");
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM gc_pending", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0, "队列出队干净");
    let _ = src_root;
}

#[test]
fn purge_drop_archive_removes_bytes() {
    // 1.0.0 真删通道：drop_archive=true 时本次归档目录随清除移除（默认归档行为不变）
    let (home, a, _b, _c) = setup();
    let mut conn = db::open(home.path()).unwrap();
    db::set_session_deleted(&conn, &a, true).unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::days(31))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    conn.execute("UPDATE sessions SET deleted_at = ?1 WHERE id = ?2",
        rusqlite::params![old, a]).unwrap();

    let kept = db::purge_session(&mut conn, home.path(), &a, false, false).unwrap();
    assert!(std::path::PathBuf::from(kept["backup_dir"].as_str().unwrap()).exists(), "默认归档保留");

    // 重建场景再删一次（另一条会话走真删）：档案不再存在
    let (home2, x, _y, _z) = setup();
    let mut conn2 = db::open(home2.path()).unwrap();
    db::set_session_deleted(&conn2, &x, true).unwrap();
    conn2.execute("UPDATE sessions SET deleted_at = ?1 WHERE id = ?2",
        rusqlite::params![old, x]).unwrap();
    let dropped = db::purge_session(&mut conn2, home2.path(), &x, false, true).unwrap();
    assert!(!std::path::PathBuf::from(dropped["backup_dir"].as_str().unwrap()).exists(), "真删后档案不保留");
}
