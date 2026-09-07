//! 0.3.1: bundle round-trip（create → verify → restore → 搜索命中）、
//! --merge 合并语义、restore-agents 写回门控、setup 一键接入门控。

// json! 只被两个 unix 门控的 stub 测试使用，Windows 编译下随之门控。
#[cfg(unix)]
use serde_json::json;
use yourmem::{adapters, bundle, db, ingest, restore, setup, vault};

const SESSION_A: &str = r#"{"type":"user","cwd":"/tmp/proj-x","uuid":"a1","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"甲项目的第一个决定"}}
{"type":"assistant","uuid":"a2","timestamp":"2026-08-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"甲项目回复"}]}}"#;

const SESSION_B: &str = r#"{"type":"user","cwd":"/tmp/proj-y","uuid":"b1","timestamp":"2026-08-02T10:00:00Z","message":{"role":"user","content":"乙项目的内涝防治讨论"}}
{"type":"assistant","uuid":"b2","timestamp":"2026-08-02T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"乙项目回复"}]}}"#;

/// 建一个有两个项目、各一个会话的库。
fn make_src() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let cdir = src.path().join("claude");
    std::fs::create_dir_all(&cdir).unwrap();
    std::fs::write(cdir.join("aaaa.jsonl"), format!("{SESSION_A}\n")).unwrap();
    std::fs::write(cdir.join("bbbb.jsonl"), format!("{SESSION_B}\n")).unwrap();
    let mut conn = db::open(home.path()).unwrap();
    let roots = vec![(adapters::AGENT_CLAUDE, src.path().join("claude"))];
    ingest::import_all(&mut conn, home.path(), &roots, None, None).unwrap();
    (home, src)
}

#[test]
fn bundle_roundtrip_fresh_home() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let bundle_path = home.path().join("move.tar.gz");

    // create
    let manifest = bundle::create(&conn, home.path(), &bundle_path, &bundle::BundleFilter::default()).unwrap();
    assert_eq!(manifest["format_version"], 1);
    assert!(manifest["schema_version"].as_i64().unwrap() >= 2);
    assert!(manifest["objects"].as_u64().unwrap() > 0);

    // verify
    let report = bundle::verify(&bundle_path).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["corrupted"].as_array().unwrap().len(), 0);

    // restore into a fresh home
    let new_home = tempfile::tempdir().unwrap();
    let r = bundle::restore(&bundle_path, new_home.path(), false).unwrap();
    assert_eq!(r["mode"], "fresh");

    // 搜索命中率与原机一致（验收标准）
    let conn2 = db::open(new_home.path()).unwrap();
    let hits = db::search(&conn2, &db::SearchOpts {
        query: "内涝".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1);
    let hits = db::search(&conn2, &db::SearchOpts {
        query: "甲项目".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 2); // user 提问 + assistant 回复各一条

    // vault export 在新库上照常工作
    let out = new_home.path().join("exp.jsonl");
    let n = vault::export_session(&conn2, new_home.path(), "claude:aaaa", &out).unwrap();
    assert_eq!(n, 2);

    // 恢复到非空目录默认拒绝
    assert!(bundle::restore(&bundle_path, new_home.path(), false).is_err());
}

#[test]
fn bundle_filter_prunes_to_one_agent_or_project() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let bundle_path = home.path().join("only-x.tar.gz");
    let filter = bundle::BundleFilter { agent: None, project: Some("proj-x".into()) };
    bundle::create(&conn, home.path(), &bundle_path, &filter).unwrap();

    let new_home = tempfile::tempdir().unwrap();
    bundle::restore(&bundle_path, new_home.path(), false).unwrap();
    let conn2 = db::open(new_home.path()).unwrap();
    let sessions = db::recent_sessions(&conn2, None, 10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["project"], "proj-x");
    // 被剪掉的项目搜不到
    let hits = db::search(&conn2, &db::SearchOpts {
        query: "内涝".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 0);
}

#[test]
fn bundle_merge_into_existing() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let bundle_path = home.path().join("all.tar.gz");
    bundle::create(&conn, home.path(), &bundle_path, &bundle::BundleFilter::default()).unwrap();

    // 目标库：已有会话 aaaa（内容更旧/相同），缺 bbbb
    let target = tempfile::tempdir().unwrap();
    {
        let src2 = tempfile::tempdir().unwrap();
        let cdir = src2.path().join("claude");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("aaaa.jsonl"), format!("{SESSION_A}\n")).unwrap();
        let mut c = db::open(target.path()).unwrap();
        let roots = vec![(adapters::AGENT_CLAUDE, cdir)];
        ingest::import_all(&mut c, target.path(), &roots, None, None).unwrap();
    }

    let r = bundle::restore(&bundle_path, target.path(), true).unwrap();
    assert_eq!(r["mode"], "merge");
    assert_eq!(r["merged"]["sessions_skipped"], 1); // aaaa 已存在且不更旧
    assert_eq!(r["merged"]["sessions_added"], 1); // bbbb 并入

    let conn2 = db::open(target.path()).unwrap();
    let sessions = db::recent_sessions(&conn2, None, 10).unwrap();
    assert_eq!(sessions.len(), 2);
    let hits = db::search(&conn2, &db::SearchOpts {
        query: "内涝".into(), project: None, agent: None, kind: None, limit: 10,
    })
    .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn merge_carries_saw_response_item() {
    // codex 评审 blocker：标志不随 merge 走的话，合并来的 codex 文件游标已就位
    // 而标志为 0，后续纯 event_msg 增量会被当新消息双录
    let (home, _src) = make_src();
    let bundle_path = home.path().join("all.tar.gz");
    {
        let conn = db::open(home.path()).unwrap();
        conn.execute("UPDATE source_files SET saw_response_item = 1", []).unwrap();
        bundle::create(&conn, home.path(), &bundle_path, &bundle::BundleFilter::default()).unwrap();
    }
    // 全新 home 恢复 --merge（目标库需先建好）
    let target = tempfile::tempdir().unwrap();
    db::open(target.path()).unwrap();
    let r = bundle::restore(&bundle_path, target.path(), true).unwrap();
    assert_eq!(r["mode"], "merge");
    let conn2 = db::open(target.path()).unwrap();
    let saw: i64 = conn2
        .query_row("SELECT COALESCE(MAX(saw_response_item), 0) FROM source_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(saw, 1, "saw_response_item 必须随 source_files 一并合并");
}

#[test]
fn merge_keeps_compact_line_no_and_archived_at() {
    // v10/v11 回归：同版本 merge 不得丢 sessions.compact_line_no（压缩点——丢了
    // 迁入机上 session precompact 静默失效）与 projects.archived_at（废弃状态——
    // 丢了已废弃项目静默复活，违反"导入/合并不复活"裁定）
    let (home, _src) = make_src();
    let bundle_path = home.path().join("all.tar.gz");
    {
        let conn = db::open(home.path()).unwrap();
        conn.execute("UPDATE sessions SET compact_line_no = 1 WHERE id = 'claude:aaaa'", []).unwrap();
        let pid: i64 = conn
            .query_row("SELECT id FROM projects WHERE path = '/tmp/proj-x'", [], |r| r.get(0))
            .unwrap();
        db::set_project_archived(&conn, pid, true).unwrap();
        bundle::create(&conn, home.path(), &bundle_path, &bundle::BundleFilter::default()).unwrap();
    }
    let target = tempfile::tempdir().unwrap();
    db::open(target.path()).unwrap();
    let r = bundle::restore(&bundle_path, target.path(), true).unwrap();
    assert_eq!(r["mode"], "merge");
    let conn2 = db::open(target.path()).unwrap();
    let cln: Option<i64> = conn2
        .query_row("SELECT compact_line_no FROM sessions WHERE id = 'claude:aaaa'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cln, Some(1), "compact_line_no 必须随 merge 保留");
    let archived: Option<String> = conn2
        .query_row("SELECT archived_at FROM projects WHERE path = '/tmp/proj-x'", [], |r| r.get(0))
        .unwrap();
    assert!(archived.is_some(), "已废弃项目 merge 后不得复活");
    // 未废弃的项目不受影响
    let active: Option<String> = conn2
        .query_row("SELECT archived_at FROM projects WHERE path = '/tmp/proj-y'", [], |r| r.get(0))
        .unwrap();
    assert!(active.is_none());
}

#[test]
fn merge_refreshes_fts_for_updated_memory_file() {
    // codex 评审 blocker：目标已有旧版原生 memory 文件（含旧 fts 行），bundle 带
    // 新版修订 merge 后，搜索必须命中新内容——只回填"缺行"的文件会继续搜到旧文
    let seed = |home: &std::path::Path, text: &str, updated: &str| {
        let conn = db::open(home).unwrap();
        let bytes = text.as_bytes();
        let hash = vault::store_bytes(home, bytes).unwrap();
        let (fid, _) = db::upsert_memory_file(&conn, "claude", "global", "/proj/CLAUDE.md", &hash).unwrap();
        db::insert_memory_revision(&conn, fid, &hash, bytes.len() as u64).unwrap();
        // 手动设 updated_at 让新旧可比（upsert 内部用 now_iso）
        conn.execute(
            "UPDATE memory_files SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![updated, fid],
        )
        .unwrap();
        db::set_memory_fts(&conn, fid, text).unwrap();
        conn
    };
    let src_home = tempfile::tempdir().unwrap();
    {
        let conn = seed(src_home.path(), "决策：新版独有内容为准", "2026-08-02T00:00:00Z");
        bundle::create(&conn, src_home.path(), &src_home.path().join("b.tar.gz"), &bundle::BundleFilter::default()).unwrap();
    }
    let target = tempfile::tempdir().unwrap();
    seed(target.path(), "决策：旧版独有内容为准", "2026-08-01T00:00:00Z");

    let r = bundle::restore(&src_home.path().join("b.tar.gz"), target.path(), true).unwrap();
    assert_eq!(r["mode"], "merge");
    assert!(r["memory_fts_backfilled"].as_u64().unwrap_or(0) >= 1, "被更新的文件要重建索引: {r}");

    let conn2 = db::open(target.path()).unwrap();
    let fresh = db::search_memory_files_fts(&conn2, "新版独有内容", 10).unwrap();
    assert!(!fresh.is_empty(), "merge 后新内容必须可搜");
    let stale = db::search_memory_files_fts(&conn2, "旧版独有内容", 10).unwrap();
    assert!(stale.is_empty(), "旧内容不该还留在索引里");
}

#[test]
fn bundle_merge_remaps_project_ids() {
    // 源库 alpha(1)/beta(2)、memory 挂在 alpha 上；目标库只先导入了 beta（beta=1）。
    // project_id 若不做重映射直插，src 的 1 会撞上 target 的 beta——静默挂错项目。
    let s_alpha = r#"{"type":"user","cwd":"/tmp/alpha","uuid":"a1","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"alpha 会话"}}"#;
    let s_beta = r#"{"type":"user","cwd":"/tmp/beta","uuid":"b1","timestamp":"2026-08-02T10:00:00Z","message":{"role":"user","content":"beta 会话"}}"#;
    let import_one = |home: &std::path::Path, dir: &std::path::Path, name: &str, body: &str| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), format!("{body}\n")).unwrap();
        let mut conn = db::open(home).unwrap();
        let roots = vec![(adapters::AGENT_CLAUDE, dir.to_path_buf())];
        ingest::import_all(&mut conn, home, &roots, None, None).unwrap();
    };

    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    import_one(home.path(), &src.path().join("c1"), "aaaa.jsonl", s_alpha);
    import_one(home.path(), &src.path().join("c2"), "bbbb.jsonl", s_beta);
    {
        let conn = db::open(home.path()).unwrap();
        let alpha_id: i64 = conn
            .query_row("SELECT id FROM projects WHERE path = '/tmp/alpha'", [], |r| r.get(0))
            .unwrap();
        db::save_memory(&conn, &db::MemoryInput {
            project_id: Some(alpha_id),
            scope: "project",
            r#type: "fact",
            content: "alpha 的架构决定",
            status: None,
            source_session_id: None,
            source_message_id: None,
        })
        .unwrap();
    }
    let bundle_path = home.path().join("all.tar.gz");
    {
        let conn = db::open(home.path()).unwrap();
        bundle::create(&conn, home.path(), &bundle_path, &bundle::BundleFilter::default()).unwrap();
    }

    // 目标库：beta 先到，拿到 id 1（与源库 alpha 的 id 相同）
    let target = tempfile::tempdir().unwrap();
    let tgt = tempfile::tempdir().unwrap();
    import_one(target.path(), &tgt.path().join("c2"), "bbbb.jsonl", s_beta);

    let r = bundle::restore(&bundle_path, target.path(), true).unwrap();
    assert_eq!(r["merged"]["memories_added"], 1);

    let conn = db::open(target.path()).unwrap();
    let proj_path: String = conn
        .query_row(
            "SELECT p.path FROM memories m JOIN projects p ON p.id = m.project_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(proj_path, "/tmp/alpha", "memory 必须仍挂在 alpha 上");
}

#[test]
fn restore_agents_gate() {
    let (home, src) = make_src();
    let conn = db::open(home.path()).unwrap();

    // 预览：路径、后果、resume 命令
    let p = restore::plan(&conn, "claude", "aaaa").unwrap();
    let target = src.path().join("claude").join("aaaa.jsonl");
    assert_eq!(p["will_write"][0].as_str().unwrap(), target.to_string_lossy());
    assert_eq!(p["resume_command"], "claude --resume aaaa");

    // 目标已存在（源文件还在）→ 默认拒绝
    assert!(restore::execute(&conn, home.path(), "claude", "aaaa", false).is_err());

    // --force：先 .bak 再重建
    std::fs::write(&target, "corrupted local copy\n").unwrap();
    let r = restore::execute(&conn, home.path(), "claude", "aaaa", true).unwrap();
    let bak = r["backup"].as_str().unwrap();
    assert!(std::path::Path::new(bak).is_file());
    assert_eq!(std::fs::read_to_string(bak).unwrap(), "corrupted local copy\n");
    // 重建内容与原始 fixture 逐字节一致
    assert_eq!(std::fs::read_to_string(&target).unwrap(), format!("{SESSION_A}\n"));

    // opencode 型会话拒绝写回
    let oc_err = restore::plan(&conn, "opencode", "ses_anything").unwrap_err();
    assert!(oc_err.to_string().contains("session not found") || oc_err.to_string().contains("SQLite"));
}

/// 伪造 yourmem 二进制的 stub（`--version` 输出 "yourmem <ver>"），用于测试版本探测。
/// 仅限 unix：#!/bin/sh + chmod 的可执行脚本在 Windows 无法执行。
#[cfg(unix)]
fn stub_version(dir: &std::path::Path, ver: &str) -> std::path::PathBuf {
    let p = dir.join(format!("yourmem-stub-{ver}"));
    std::fs::write(&p, format!("#!/bin/sh\necho \"yourmem {ver}\"\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

// 依赖 stub_version 的 sh 脚本，Windows 无法执行，整测试门控 unix。
#[cfg(unix)]
#[test]
fn setup_gate_plan_and_execute() {
    let fake = tempfile::tempdir().unwrap();
    // 模拟本机装了 claude + codex
    std::fs::create_dir_all(fake.path().join(".claude")).unwrap();
    std::fs::create_dir_all(fake.path().join(".codex")).unwrap();
    let targets = setup::Targets {
        claude_json: fake.path().join(".claude.json"),
        claude_md: fake.path().join(".claude").join("CLAUDE.md"),
        codex_config: fake.path().join(".codex").join("config.toml"),
        codex_agents: fake.path().join(".codex").join("AGENTS.md"),
        hermes_config: fake.path().join("nope-hermes.yaml"),
    };

    // 预览：claude+codex 各两个 todo 动作（hermes 未装 → skip，0.3.9 起入预览清单）
    let p = setup::plan(&targets).unwrap();
    let agents = p["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 3);
    assert_eq!(agents.iter().filter(|a| a["status"] == "detected").count(), 2);
    assert!(agents.iter().filter(|a| a["status"] == "detected")
        .all(|a| a["actions"].as_array().unwrap().iter().all(|act| act["status"] == "todo")));

    // 执行：MCP 注册 + 指令写入
    let r = setup::execute(&targets).unwrap();
    assert!(r["agents"].as_array().unwrap().iter().filter(|a| a["status"] == "detected").all(|a| a["status"] == "configured"));
    let claude_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&targets.claude_json).unwrap()).unwrap();
    // current_exe 在测试里是测试二进制，只验证条目非空且 args 正确
    assert!(!claude_json["mcpServers"]["yourmem"]["command"].as_str().unwrap().is_empty());
    assert_eq!(claude_json["mcpServers"]["yourmem"]["args"][0], "mcp");
    assert!(std::fs::read_to_string(&targets.codex_config).unwrap().contains("[mcp_servers.yourmem]"));
    assert!(std::fs::read_to_string(&targets.claude_md).unwrap().contains("yourmem 跨 agent 记忆库"));
    assert!(std::fs::read_to_string(&targets.codex_agents).unwrap().contains("search_history"));

    // 幂等：把注册命令换成同版本 stub 后再跑，应全是 already_done
    //（测试二进制自身的 --version 不是 yourmem 的输出格式，直接复跑会判 stale）
    let stub = stub_version(fake.path(), env!("CARGO_PKG_VERSION"));
    {
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&targets.claude_json).unwrap()).unwrap();
        v["mcpServers"]["yourmem"]["command"] = json!(stub.to_str().unwrap());
        std::fs::write(&targets.claude_json, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
    std::fs::write(
        &targets.codex_config,
        format!(
            "[mcp_servers.yourmem]\ncommand = {:?}\nargs = [\"mcp\"]\n",
            stub.to_str().unwrap()
        ),
    )
    .unwrap();
    let r2 = setup::execute(&targets).unwrap();
    assert!(r2["agents"].as_array().unwrap().iter()
        .filter(|a| a["status"] == "detected")
        .flat_map(|a| a["actions"].as_array().unwrap().clone())
        .all(|act| act["status"] == "already_done"));

    // 已有内容被保留且先落 .bak：清掉 marker 让动作重新 todo
    std::fs::write(&targets.claude_md, "我自己的规则\n").unwrap();
    setup::execute(&targets).unwrap();
    let content = std::fs::read_to_string(&targets.claude_md).unwrap();
    assert!(content.starts_with("我自己的规则"));
    assert!(content.contains("yourmem 跨 agent 记忆库"));
    let baks: Vec<_> = std::fs::read_dir(targets.claude_md.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("CLAUDE.md.bak-"))
        .collect();
    assert!(!baks.is_empty(), "覆盖前必须留下 .bak 时间戳备份");
}

// 依赖 stub_version 的 sh 脚本，Windows 无法执行，整测试门控 unix。
#[cfg(unix)]
#[test]
fn setup_detects_and_refreshes_stale_mcp_registration() {
    // 真机教训（2026-08-23）：手动注册的 0.1.0 旧二进制被 setup 的"已配置"检测放过，
    // agent 调 MCP 全部落空。现在 plan 必须把版本不一致判为 stale，execute 重注册。
    let fake = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fake.path().join(".claude")).unwrap();
    std::fs::create_dir_all(fake.path().join(".codex")).unwrap();
    let targets = setup::Targets {
        claude_json: fake.path().join(".claude.json"),
        claude_md: fake.path().join(".claude").join("CLAUDE.md"),
        codex_config: fake.path().join(".codex").join("config.toml"),
        codex_agents: fake.path().join(".codex").join("AGENTS.md"),
        hermes_config: fake.path().join("nope-hermes.yaml"),
    };
    let old = stub_version(fake.path(), "0.0.1");
    std::fs::write(
        &targets.claude_json,
        json!({ "mcpServers": { "yourmem": { "command": old.to_str().unwrap(), "args": ["mcp"] } } }).to_string(),
    )
    .unwrap();
    std::fs::write(
        &targets.codex_config,
        format!(
            "[mcp_servers.yourmem]\ncommand = {:?}\nargs = [\"mcp\"]\n\n[mcp_servers.yourmem.tools.search_history]\napproval_mode = \"approve\"\n",
            old.to_str().unwrap()
        ),
    )
    .unwrap();

    let p = setup::plan(&targets).unwrap();
    for a in p["agents"].as_array().unwrap().into_iter().filter(|a| a["status"] == "detected") {
        let mcp = &a["actions"][0];
        assert_eq!(mcp["status"], "stale", "{} 的 MCP 注册应判 stale", a["agent"]);
        assert_eq!(mcp["detail"]["version"], "0.0.1");
    }

    setup::execute(&targets).unwrap();
    // 重注册后命令不再指向旧 stub，codex 的 tools 子表保留
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&targets.claude_json).unwrap()).unwrap();
    let new_cmd = v["mcpServers"]["yourmem"]["command"].as_str().unwrap();
    assert_ne!(new_cmd, old.to_str().unwrap());
    let toml = std::fs::read_to_string(&targets.codex_config).unwrap();
    assert!(!toml.contains(old.to_str().unwrap()));
    assert!(toml.contains("[mcp_servers.yourmem.tools.search_history]"), "tools 子表必须保留");
    assert!(toml.matches("[mcp_servers.yourmem]").count() == 1, "主表不能重复");
}

#[test]
fn usage_log_counts() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    db::log_usage(&conn, "cli", "search").unwrap();
    db::log_usage(&conn, "cli", "search").unwrap();
    db::log_usage(&conn, "mcp", "search_history").unwrap();
    let summary = db::usage_summary(&conn, 7).unwrap();
    assert_eq!(summary.len(), 2);
    assert_eq!(summary[0]["count"], 2); // 按次数倒序
    let stats = db::stats(&conn).unwrap();
    assert!(stats["usage_last_7d"].is_array());
}

#[test]
fn instruction_block_version_gate() {
    let fake = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fake.path().join(".claude")).unwrap();
    let targets = setup::Targets {
        claude_json: fake.path().join(".claude.json"),
        claude_md: fake.path().join(".claude").join("CLAUDE.md"),
        codex_config: fake.path().join(".codex").join("config.toml"),
        codex_agents: fake.path().join(".codex").join("AGENTS.md"),
        hermes_config: fake.path().join("nope-hermes.yaml"),
    };
    // 只有 claude 目录存在 → codex skip；指令文件含旧版块（有 marker、无新全文），
    // 且 marker 前后都有用户自己的内容
    std::fs::write(
        &targets.claude_md,
        "# 我自己的规则\n\n## yourmem 跨 agent 记忆库（MCP）\n\n旧版内容，没有止损规则\n\n## 我自己的另一节\n保留我\n",
    )
    .unwrap();

    // plan：旧版块判 stale
    let p = setup::plan(&targets).unwrap();
    let claude = &p["agents"].as_array().unwrap()[0];
    let instr = claude["actions"].as_array().unwrap()
        .iter().find(|a| a["kind"] == "global_instructions").unwrap();
    assert_eq!(instr["status"], "stale");

    // execute：旧块整体替换为新全文，用户内容保留，旧内容消失，写前有 .bak
    setup::execute(&targets).unwrap();
    let content = std::fs::read_to_string(&targets.claude_md).unwrap();
    assert!(content.contains(setup::INSTRUCTION_BLOCK.trim()));
    assert!(content.starts_with("# 我自己的规则"));
    assert!(content.contains("## 我自己的另一节\n保留我"));
    assert!(!content.contains("旧版内容"));
    assert!(std::fs::read_dir(targets.claude_md.parent().unwrap()).unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with("CLAUDE.md.bak-")));

    // 幂等：已是新全文 → already_done
    let r2 = setup::execute(&targets).unwrap();
    let instr2 = r2["agents"].as_array().unwrap()[0]["actions"].as_array().unwrap()
        .iter().find(|a| a["kind"] == "global_instructions").unwrap().clone();
    assert_eq!(instr2["status"], "already_done");
}

#[test]
fn filtered_bundle_keeps_only_own_agent_tombstones() {
    // codex 十审：agent 过滤只留本 agent 墓碑；有效墓碑（会话已 purge，
    // sessions 里没有）不得被"孤儿清理"误删
    let (home, src) = make_src();
    let conn = db::open(home.path()).unwrap();
    // 墓碑之一用"稍后要导入的 kimi wire.jsonl 的真实路径"——恢复后按路径
    // 精确阻断的效果才能被真正检验（codex 终审 suggestion）
    let victim_wire = src.path().join("kimi-victim").join("sessions").join("wd_v")
        .join("session_00000000-0000-0000-0000-0000000000ee").join("agents").join("main").join("wire.jsonl");
    std::fs::create_dir_all(victim_wire.parent().unwrap()).unwrap();
    std::fs::write(&victim_wire,
        r#"{"type":"turn.prompt","input":[{"type":"text","text":"同路径墓碑的受害者"}]}"#.to_string() + "\n").unwrap();
    conn.execute(
        "INSERT INTO purged_sources(path, session_id, purged_at) VALUES
         ('/tmp/a.jsonl', 'claude:purged-1', '2026-08-23T00:00:00Z'),
         (?1,            'kimi:session_00000000-0000-0000-0000-0000000000ee', '2026-08-23T00:00:00Z'),
         ('/tmp/o.db',   'opencode:ses_purged', '2026-08-23T00:00:00Z')",
        rusqlite::params![victim_wire.to_string_lossy().as_ref()],
    )
    .unwrap();
    // 前置断言：未过滤的 bundle（带墓碑）恢复后，同路径导入为 0（墓碑确实阻断）
    let out_all = home.path().join("all-with-tomb.tar.gz");
    bundle::create(&conn, home.path(), &out_all, &bundle::BundleFilter::default()).unwrap();
    let t_all = tempfile::tempdir().unwrap();
    bundle::restore(&out_all, t_all.path(), false).unwrap();
    {
        let mut ca = db::open(t_all.path()).unwrap();
        let roots_a = vec![(adapters::AGENT_KIMI, src.path().join("kimi-victim").join("sessions"))];
        let out_a = yourmem::ingest::import_all(&mut ca, t_all.path(), &roots_a, None, None).unwrap();
        assert_eq!(out_a.messages_added, 0, "前置：真实路径墓碑确实阻断导入");
    }
    let out = home.path().join("only-claude.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter { agent: Some("claude".into()), project: None }).unwrap();

    let target = tempfile::tempdir().unwrap();
    bundle::restore(&out, target.path(), false).unwrap();
    let c2 = db::open(target.path()).unwrap();
    let claude_tb: i64 = c2.query_row(
        "SELECT COUNT(*) FROM purged_sources WHERE session_id LIKE 'claude:%'", [], |r| r.get(0)).unwrap();
    let oc_tb: i64 = c2.query_row(
        "SELECT COUNT(*) FROM purged_sources WHERE session_id LIKE 'opencode:%'", [], |r| r.get(0)).unwrap();
    assert_eq!(claude_tb, 1, "本 agent 的有效墓碑必须保留（会话不在 sessions 是常态）");
    assert_eq!(oc_tb, 0, "其他 agent 的墓碑不得进过滤 bundle");

    // project 过滤不裁墓碑（保守多带）
    let out2 = home.path().join("only-x2.tar.gz");
    bundle::create(&conn, home.path(), &out2, &bundle::BundleFilter { agent: None, project: Some("proj-x".into()) }).unwrap();
    let t2 = tempfile::tempdir().unwrap();
    bundle::restore(&out2, t2.path(), false).unwrap();
    let c3 = db::open(t2.path()).unwrap();
    let kept: i64 = c3.query_row("SELECT COUNT(*) FROM purged_sources", [], |r| r.get(0)).unwrap();
    assert_eq!(kept, 0, "project 过滤全删墓碑——多带会静默阻断他项目本地导入（codex 十一审复现）");
    // 真实导入验证：project 过滤 bundle 恢复后，同一路径的源文件不得被墓碑阻断
    let mut c4 = db::open(t2.path()).unwrap();
    let roots = vec![(adapters::AGENT_KIMI, src.path().join("kimi-victim").join("sessions"))];
    let out3 = yourmem::ingest::import_all(&mut c4, t2.path(), &roots, None, None).unwrap();
    assert_eq!(out3.messages_added, 1, "墓碑已随 project 过滤删除，同路径源文件正常导入");
}

#[test]
fn merge_bundle_without_tombstone_table_v3_like() {
    // 模拟缺 purged_sources 表的 v3-like bundle（保留 deleted_at 列；真实 v3 的缺列分支由 0.3.1.1 的 manifest 门与既有 PRAGMA 探测处理）——merge 必须探测跳过而非报错
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let out = home.path().join("v3-like.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
    // 解包、删表、重打包，模拟 v3 bundle
    let tmp = tempfile::tempdir().unwrap();
    let file = std::fs::File::open(&out).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(dec);
    ar.unpack(tmp.path()).unwrap();
    let root: std::path::PathBuf = std::fs::read_dir(tmp.path()).unwrap().next().unwrap().unwrap().path();
    let snap = root.join("db");
    let snap_file: std::path::PathBuf = std::fs::read_dir(&snap).unwrap().next().unwrap().unwrap().path();
    {
        let c = rusqlite::Connection::open(&snap_file).unwrap();
        // 真实 v3 快照也没有 v10/v11 的两列——一并去掉，覆盖 merge 的缺列探测分支
        c.execute_batch(
            "DROP TABLE purged_sources; DROP TABLE gc_pending;
             ALTER TABLE sessions DROP COLUMN compact_line_no;
             ALTER TABLE projects DROP COLUMN archived_at;",
        ).unwrap();
        c.pragma_update(None, "user_version", 3).unwrap();
    }
    // manifest 同步改 schema_version=3（版本门读 manifest，快照 user_version
    // 只是模拟的陪衬——codex 十一审 suggestion）
    {
        let mp = root.join("manifest.json");
        let mut m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&mp).unwrap()).unwrap();
        m["schema_version"] = serde_json::json!(3);
        std::fs::write(&mp, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    }
    let out2 = home.path().join("v3-real.tar.gz");
    let f2 = std::fs::File::create(&out2).unwrap();
    let enc = flate2::write::GzEncoder::new(f2, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(root.file_name().unwrap(), &root).unwrap();
    tar.into_inner().unwrap().finish().unwrap();

    let target = tempfile::tempdir().unwrap();
    let tc = db::open(target.path()).unwrap();
    db::save_memory(&tc, &db::MemoryInput {
        project_id: None, scope: "global", r#type: "fact",
        content: "目标库已有内容", status: None,
        source_session_id: None, source_message_id: None,
    }).unwrap();
    let r = bundle::restore(&out2, target.path(), true).unwrap();
    assert_eq!(r["mode"], "merge", "v3（无墓碑表）bundle merge 不得报错");
}

#[test]
fn merge_remaps_memory_source_message_pointer() {
    // 终审 blocker：merge 重插消息 rowid 变化，memories.source_message_id
    // 必须跟着重映射——否则错挂到目标库已有消息
    let (home, src) = make_src();
    let conn = db::open(home.path()).unwrap();
    // 给 claude:aaaa 的第一条消息挂一条 memory（来源指针）
    let msg_id: i64 = conn.query_row(
        "SELECT id FROM messages WHERE session_id = 'claude:aaaa' ORDER BY line_no, ord LIMIT 1",
        [], |r| r.get(0)).unwrap();
    db::save_memory(&conn, &db::MemoryInput {
        project_id: None, scope: "global", r#type: "decision",
        content: "merge 指针重映射验证", status: None,
        source_session_id: Some("claude:aaaa"), source_message_id: Some(msg_id),
    }).unwrap();

    let out = home.path().join("ptr.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
    // 目标库预置一条消息占住 rowid 空间，逼 bundle 消息拿新 id
    let target = tempfile::tempdir().unwrap();
    {
        let src2 = tempfile::tempdir().unwrap();
        let cdir = src2.path().join("claude");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("zzzz.jsonl"),
            r#"{"type":"user","cwd":"/tmp/zz","uuid":"z1","timestamp":"2026-08-23T08:00:00Z","message":{"role":"user","content":"占位消息"}}"#.to_string() + "\n").unwrap();
        let mut c = db::open(target.path()).unwrap();
        let roots = vec![(adapters::AGENT_CLAUDE, cdir)];
        yourmem::ingest::import_all(&mut c, target.path(), &roots, None, None).unwrap();
    }
    bundle::restore(&out, target.path(), true).unwrap();

    let tc = db::open(target.path()).unwrap();
    let (smid, line): (Option<i64>, Option<i64>) = tc.query_row(
        "SELECT m.source_message_id, msg.line_no FROM memories m
         LEFT JOIN messages msg ON msg.id = m.source_message_id
         WHERE m.content = 'merge 指针重映射验证'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert!(smid.is_some(), "指针不得丢");
    assert_eq!(line, Some(1), "重映射后必须指向 aaaa 的 L1（原消息行号），实际 {line:?}");
    // JOIN 语义本身由 tests/dossier.rs 的行号保护测试覆盖；此处证明重映射即可
    let _ = src;
}

#[test]
fn merge_session_clears_conflicting_tombstone() {
    // 终审 blocker：bundle 会话进入目标库时清除同名墓碑（显式迁移意图优先）
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let out = home.path().join("tbt.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();

    let target = tempfile::tempdir().unwrap();
    let tc = db::open(target.path()).unwrap();
    tc.execute(
        "INSERT INTO purged_sources(path, session_id, purged_at) VALUES ('/x/a.jsonl', 'claude:aaaa', '2026-08-01T00:00:00Z')",
        [],
    ).unwrap();
    drop(tc);
    bundle::restore(&out, target.path(), true).unwrap();
    let tc = db::open(target.path()).unwrap();
    let tb: i64 = tc.query_row(
        "SELECT COUNT(*) FROM purged_sources WHERE session_id = 'claude:aaaa'", [], |r| r.get(0)).unwrap();
    assert_eq!(tb, 0, "bundle 带入的会话必须清掉矛盾墓碑");
    let sess: i64 = tc.query_row("SELECT COUNT(*) FROM sessions WHERE id = 'claude:aaaa'", [], |r| r.get(0)).unwrap();
    assert_eq!(sess, 1);
}

#[test]
fn merge_pointer_survives_skipped_and_replaced_sessions() {
    // 终审二轮两路径：skipped 会话的 memory 指针不得变 NULL（按坐标对齐）；
    // 替换会话上既有 memory 的指针不得悬挂（按坐标重指新行）
    let (home, src) = make_src(); // aaaa/bbbb 各 2 条消息
    let conn = db::open(home.path()).unwrap();
    let out = home.path().join("ptr2.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();

    // 目标库：先整库恢复一次（aaaa/bbbb 都进来），挂一条指向 aaaa L2 的 memory
    let target = tempfile::tempdir().unwrap();
    bundle::restore(&out, target.path(), false).unwrap();
    {
        let tc = db::open(target.path()).unwrap();
        // 两条既有指针分别指向 aaaa 的 L1 与 L2（rowid 复用下按旧消息 id 更新
        // 会把两条改成同一目标——终审四轮 suggestion 的尖锐回归）
        for (line, tag) in [(1, "既有指针L1"), (2, "既有指针L2")] {
            let mid: i64 = tc.query_row(
                "SELECT id FROM messages WHERE session_id = 'claude:aaaa' AND line_no = ?1",
                rusqlite::params![line], |r| r.get(0)).unwrap();
            db::save_memory(&tc, &db::MemoryInput {
                project_id: None, scope: "global", r#type: "decision",
                content: tag, status: None,
                source_session_id: Some("claude:aaaa"), source_message_id: Some(mid),
            }).unwrap();
        }
    }

    // 源库：给 aaaa 追加消息（旧文件追加一行 → bundle 侧 aaaa 更新 → 替换路径），
    // 再挂一条指向 bbbb L1 的 memory（bbbb 不变 → skipped 路径）
    {
        std::fs::write(
            src.path().join("claude").join("aaaa.jsonl"),
            std::fs::read_to_string(src.path().join("claude").join("aaaa.jsonl")).unwrap()
                + r#"{"type":"user","cwd":"/tmp/proj-x","uuid":"a3","timestamp":"2026-08-01T11:00:00Z","message":{"role":"user","content":"甲项目追加行"}}"# + "\n",
        ).unwrap();
        let mut c = db::open(home.path()).unwrap();
        let roots = vec![(adapters::AGENT_CLAUDE, src.path().join("claude"))];
        yourmem::ingest::import_all(&mut c, home.path(), &roots, None, None).unwrap();
        let mid: i64 = c.query_row(
            "SELECT id FROM messages WHERE session_id = 'claude:bbbb' AND line_no = 1",
            [], |r| r.get(0)).unwrap();
        db::save_memory(&c, &db::MemoryInput {
            project_id: None, scope: "global", r#type: "decision",
            content: "skipped 会话指针（不得变 NULL）", status: None,
            source_session_id: Some("claude:bbbb"), source_message_id: Some(mid),
        }).unwrap();
        drop(c);
    }
    // 第二次合并：aaaa 走替换、bbbb 走 skipped
    bundle::create(&db::open(home.path()).unwrap(), home.path(),
        &home.path().join("ptr3.tar.gz"), &bundle::BundleFilter::default()).unwrap();
    bundle::restore(&home.path().join("ptr3.tar.gz"), target.path(), true).unwrap();

    let tc = db::open(target.path()).unwrap();
    // skipped：指针仍指向 bbbb 的 L1
    let (smid, sess, line): (Option<i64>, Option<String>, Option<i64>) = tc.query_row(
        "SELECT m.source_message_id, msg.session_id, msg.line_no FROM memories m
         LEFT JOIN messages msg ON msg.id = m.source_message_id
         WHERE m.content = 'skipped 会话指针（不得变 NULL）'",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!((sess.as_deref(), line), (Some("claude:bbbb"), Some(1)), "skipped 指针按坐标对齐: {sess:?}/{line:?}");
    assert!(smid.is_some());
    // replaced：两条既有指针各自重指到原行号（rowid 复用不改坐标语义）
    for (want_line, tag) in [(1, "既有指针L1"), (2, "既有指针L2")] {
        let (sess, line): (Option<String>, Option<i64>) = tc.query_row(
            "SELECT msg.session_id, msg.line_no FROM memories m
             JOIN messages msg ON msg.id = m.source_message_id
             WHERE m.content = ?1",
            rusqlite::params![tag],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!((sess.as_deref(), line), (Some("claude:aaaa"), Some(want_line)),
            "{tag} 替换后必须仍指向 L{want_line}（rowid 复用不得改坐标）");
    }
}

// S9 回归：同一 bundle 重复 --merge（常见误操作），handoffs 不得逐批翻倍
#[test]
fn merge_same_bundle_twice_dedups_handoffs() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let pid: i64 = conn
        .query_row("SELECT id FROM projects ORDER BY id LIMIT 1", [], |r| r.get(0))
        .unwrap();
    db::create_handoff(&conn, pid, &db::HandoffFields {
        title: "迁移前交接", done: "d", state: "s", decisions: "",
        files_changed: "", open_issues: "", next_steps: "", session_id: None,
    })
    .unwrap();
    let bundle_path = home.path().join("all.tar.gz");
    bundle::create(&conn, home.path(), &bundle_path, &bundle::BundleFilter::default()).unwrap();

    let target = tempfile::tempdir().unwrap();
    db::open(target.path()).unwrap(); // merge 要求目标库已存在
    bundle::restore(&bundle_path, target.path(), true).unwrap();
    bundle::restore(&bundle_path, target.path(), true).unwrap();

    let conn2 = db::open(target.path()).unwrap();
    let n: i64 = conn2
        .query_row("SELECT COUNT(*) FROM handoffs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "同一 bundle 重复 merge 不得翻倍 handoffs");
}

// S12 回归：usage_summary 窗口边界——ts 是 RFC3339（'T' 分隔），旧实现用
// datetime('now')（空格分隔）做字符串比较，'T' > ' ' 导致截止日更早的记录
// 也被算进窗口（"7 天"实为 8 天）
#[test]
fn usage_summary_window_excludes_before_cutoff() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(home.path()).unwrap();
    conn.execute(
        "INSERT INTO usage_log(ts, source, name)
         VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now','-7 days','-1 hour'), 'cli', 'old_event')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_log(ts, source, name)
         VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now','-6 days'), 'cli', 'recent_event')",
        [],
    )
    .unwrap();
    let summary = db::usage_summary(&conn, 7).unwrap();
    assert_eq!(summary.len(), 1, "截止点之前的记录不得入窗: {summary:?}");
    assert_eq!(summary[0]["name"], "recent_event");
}


/// Rewrite only synthetic test archives to exercise validation independently of creation.
fn rewrite_bundle(path: &std::path::Path, change: impl FnOnce(&std::path::Path, &rusqlite::Connection)) {
    let tmp = tempfile::tempdir().unwrap();
    let dec = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
    tar::Archive::new(dec).unpack(tmp.path()).unwrap();
    let root = std::fs::read_dir(tmp.path()).unwrap().next().unwrap().unwrap().path();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("manifest.json")).unwrap()).unwrap();
    let db_path = root.join("db").join(manifest["db_snapshot"].as_str().unwrap());
    let conn = rusqlite::Connection::open(db_path).unwrap();
    change(&root, &conn);
    drop(conn);
    let enc = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::default());
    let mut archive = tar::Builder::new(enc);
    archive.append_dir_all(root.file_name().unwrap(), &root).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
}

#[test]
fn project_bundle_excludes_other_project_and_global_memory_bytes() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    for (scope, path, text) in [
        ("project:/tmp/proj-x", "/tmp/proj-x/AGENTS.md", "KEEP_PROJECT_MEMORY"),
        ("project:/tmp/proj-y", "/tmp/proj-y/AGENTS.md", "EXCLUDED_PROJECT_MEMORY"),
        ("global", "/tmp/global/AGENTS.md", "EXCLUDED_GLOBAL_MEMORY"),
    ] {
        let hash = vault::store_bytes(home.path(), text.as_bytes()).unwrap();
        let (fid, _) = db::upsert_memory_file(&conn, "codex", scope, path, &hash).unwrap();
        db::insert_memory_revision(&conn, fid, &hash, text.len() as u64).unwrap();
        db::set_memory_fts(&conn, fid, text).unwrap();
    }
    conn.execute("INSERT INTO memories(id,scope,type,content,created_at,updated_at)
        VALUES ('global-test','global','fact','EXCLUDED_CURATED_GLOBAL','2026-01-01','2026-01-01')", []).unwrap();
    let out = home.path().join("project.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter { project: Some("proj-x".into()), agent: None }).unwrap();
    let target = tempfile::tempdir().unwrap();
    bundle::restore(&out, target.path(), false).unwrap();
    let restored = db::open(target.path()).unwrap();
    assert_eq!(db::list_memory_files(&restored).unwrap().len(), 1);
    assert_eq!(restored.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    let dec = flate2::read::GzDecoder::new(std::fs::File::open(out).unwrap());
    for entry in tar::Archive::new(dec).entries().unwrap() {
        use std::io::Read;
        let mut bytes = Vec::new();
        entry.unwrap().read_to_end(&mut bytes).unwrap();
        for excluded in ["EXCLUDED_PROJECT_MEMORY", "EXCLUDED_GLOBAL_MEMORY", "EXCLUDED_CURATED_GLOBAL"] {
            assert!(!bytes.windows(excluded.len()).any(|w| w == excluded.as_bytes()), "excluded text in archive: {excluded}");
        }
    }
}

#[test]
fn bundle_validation_checks_reference_coverage_schema_and_database_integrity() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    for case in ["missing", "schema", "foreign-key", "invalid-path"] {
        let out = home.path().join(format!("{case}.tar.gz"));
        bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
        rewrite_bundle(&out, |root, c| match case {
            "missing" => { c.execute("UPDATE vault_lines SET hash=?1", ["f".repeat(64)]).unwrap(); }
            "schema" => { c.pragma_update(None, "user_version", 999).unwrap(); }
            "foreign-key" => { c.execute_batch("PRAGMA foreign_keys=OFF; UPDATE messages SET session_id='missing' WHERE session_id='claude:aaaa';").unwrap(); }
            _ => {
                let mp = root.join("manifest.json");
                let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&mp).unwrap()).unwrap();
                m["db_snapshot"] = serde_json::json!("../outside.sqlite");
                std::fs::write(mp, serde_json::to_vec(&m).unwrap()).unwrap();
            }
        });
        match bundle::verify(&out) {
            Ok(r) => assert_eq!(r["ok"], false, "{case}"),
            Err(_) => assert_eq!(case, "invalid-path"),
        }
        let target = tempfile::tempdir().unwrap();
        assert!(bundle::restore(&out, target.path(), false).is_err());
        assert!(!target.path().join("yourmem.db").exists());
    }
}

#[test]
fn object_copy_failure_preserves_database_and_allows_restore_retry() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let hash: String = conn.query_row("SELECT hash FROM vault_lines LIMIT 1", [], |r| r.get(0)).unwrap();
    let out = home.path().join("restore.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
    for merge in [false, true] {
        let target = tempfile::tempdir().unwrap();
        if merge { db::open(target.path()).unwrap(); }
        std::fs::create_dir_all(target.path().join("objects")).unwrap();
        let blocker = target.path().join("objects").join(&hash[..2]);
        std::fs::write(&blocker, "synthetic obstruction").unwrap();
        assert!(bundle::restore(&out, target.path(), merge).is_err());
        if merge {
            let c = db::open(target.path()).unwrap();
            assert_eq!(db::recent_sessions(&c, None, 10).unwrap().len(), 0);
        } else { assert!(!target.path().join("yourmem.db").exists()); }
        std::fs::remove_file(blocker).unwrap();
        bundle::restore(&out, target.path(), merge).unwrap();
        let c = db::open(target.path()).unwrap();
        assert_eq!(db::recent_sessions(&c, None, 10).unwrap().len(), 2);
        assert!(vault::read_object(target.path(), &hash, true).is_ok());
    }
}

#[test]
fn restore_repairs_corrupt_existing_objects_and_previews_conflicts() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let out = home.path().join("repair.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
    let target = tempfile::tempdir().unwrap();
    bundle::restore(&out, target.path(), false).unwrap();
    let hash: String = conn.query_row("SELECT hash FROM vault_lines LIMIT 1", [], |r| r.get(0)).unwrap();
    std::fs::write(vault::object_path(target.path(), &hash), "damaged").unwrap();
    let preview = bundle::restore_plan(&out, target.path()).unwrap();
    assert_eq!(preview["merge_plan"]["sessions_skipped"], 2);
    bundle::restore(&out, target.path(), true).unwrap();
    assert!(vault::read_object(target.path(), &hash, true).is_ok());
}

#[test]
fn cli_restore_into_default_fresh_home_does_not_create_database_early() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let out = home.path().join("cli.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
    let target = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_yourmem"))
        .env("YOUMEM_HOME", target.path()).arg("bundle").arg("restore").arg(&out).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let r: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(r["mode"], "fresh");
}


#[test]
fn failed_index_backfill_rolls_back_the_entire_merge() {
    let (home, _src) = make_src();
    let conn = db::open(home.path()).unwrap();
    let hash = vault::store_bytes(home.path(), b"source memory").unwrap();
    let (fid, _) = db::upsert_memory_file(&conn, "codex", "global", "/tmp/AGENTS.md", &hash).unwrap();
    db::insert_memory_revision(&conn, fid, &hash, 13).unwrap();
    db::set_memory_fts(&conn, fid, "source memory").unwrap();
    let out = home.path().join("transaction.tar.gz");
    bundle::create(&conn, home.path(), &out, &bundle::BundleFilter::default()).unwrap();
    let target = tempfile::tempdir().unwrap();
    let target_conn = db::open(target.path()).unwrap();
    target_conn.execute_batch("DROP TABLE memory_fts; CREATE TABLE memory_fts(broken TEXT);").unwrap();
    assert!(bundle::restore(&out, target.path(), true).is_err());
    assert_eq!(target_conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(target_conn.query_row("SELECT COUNT(*) FROM memory_files", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    target_conn.execute_batch("DROP TABLE memory_fts;").unwrap();
    drop(target_conn);
    db::open(target.path()).unwrap();
    bundle::restore(&out, target.path(), true).unwrap();
    let c = db::open(target.path()).unwrap();
    assert_eq!(yourmem::memfiles::search(&c, target.path(), "source memory", 10).unwrap().len(), 1);
}
