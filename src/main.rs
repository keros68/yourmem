use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use yourmem::{adapters, data_home, db, doctor, expand_home, ingest, vault};

#[derive(Parser)]
#[command(name = "yourmem", version, about = "Local-first session vault, memory and recall for AI coding agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Discover, import and archive agent session files (incremental).
    Import {
        /// Override Claude Code sessions root (default ~/.claude/projects).
        #[arg(long)]
        claude_dir: Option<PathBuf>,
        /// Override Codex sessions root (default ~/.codex/sessions).
        #[arg(long)]
        codex_dir: Option<PathBuf>,
        /// Override ZCode rollout root (default ~/.zcode/cli/rollout).
        #[arg(long)]
        zcode_dir: Option<PathBuf>,
        /// Override Kimi Code sessions root (default ~/.kimi-code/sessions).
        #[arg(long)]
        kimi_dir: Option<PathBuf>,
        /// Override OpenCode database (default ~/.local/share/opencode/opencode.db).
        #[arg(long)]
        opencode_db: Option<PathBuf>,
        /// Override Hermes database (default ~/.hermes/state.db).
        #[arg(long)]
        hermes_db: Option<PathBuf>,
    },
    /// Continuously import every N seconds (default 5).
    Watch {
        #[arg(long, default_value = "5")]
        interval: u64,
    },
    /// Full-text search across all imported sessions.
    Search {
        query: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// List known projects; subcommands add/archive/restore (bare = list active).
    Projects {
        #[command(subcommand)]
        cmd: Option<ProjectsCmd>,
    },
    /// Compact context for one project (sessions + memories + latest handoff).
    Context {
        /// Project name or path fragment (default: current directory's project).
        project: Option<String>,
        /// 可读 markdown 一页（贴进不支持 MCP 的 AI，如 ChatGPT 网页版）。
        #[arg(long)]
        markdown: bool,
    },
    /// Session detail and trash governance (soft-delete is recoverable).
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Project dossier (§7.2): deterministic aggregate view with source pointers.
    /// Default JSON; --markdown / -o for wiki-style export.
    Dossier {
        /// Project name or path fragment (default: current directory's project).
        project: Option<String>,
        #[arg(long)]
        markdown: bool,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Daily digest (§7.3): today's sessions/projects/memories/artifacts card.
    Digest {
        /// Day as YYYY-MM-DD (default: today, local time).
        day: Option<String>,
        #[arg(long)]
        markdown: bool,
    },
    /// Memory: curated project knowledge with provenance and lifecycle.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Native agent memory file backups (MEMORY.md / AGENTS.md revisions).
    MemoryFiles {
        #[command(subcommand)]
        cmd: MemoryFilesCmd,
    },
    /// Files produced by sessions (from file-writing tool calls).
    Artifacts {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// Handoffs: create or show.
    Handoff {
        #[command(subcommand)]
        cmd: HandoffCmd,
    },
    /// Bundle: one-shot migration backup/restore (.tar.gz). 搬家，不是同步。
    Bundle {
        #[command(subcommand)]
        cmd: BundleCmd,
    },
    /// 写回 agent 数据目录（一次性迁移，知情门控：预览→确认→备份→执行）。
    /// 仅限文件型 agent（claude/codex）；opencode 等 SQLite 型不支持写回。
    RestoreAgents {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        session: String,
        /// 目标已存在时仍写入（自动先做 .bak-YYYYMMDD-HHMMSS 备份）。
        #[arg(long)]
        force: bool,
        /// 跳过交互确认（脚本用；确认信息打印在 stderr）。
        #[arg(long)]
        yes: bool,
    },
    /// Vault: immutable backup status, export, and DB snapshots.
    Backup {
        #[command(subcommand)]
        cmd: BackupCmd,
    },
    /// 一键接入本机 agent（注册 MCP + 写全局指令；预览→确认→.bak→执行，幂等）。
    Setup {
        /// 跳过交互确认（脚本用）。
        #[arg(long)]
        yes: bool,
    },
    /// Agent 数据源：检测五源 + 管理自定义采集根（config.json）。
    Agents {
        #[command(subcommand)]
        cmd: AgentsCmd,
    },
    /// 搜索索引范围（1.0.1 轻量化）：工具输出默认不索引，status/enable-tools/
    /// disable-tools/compact 管理模式与磁盘占用。
    Index {
        #[command(subcommand)]
        cmd: IndexCmd,
    },
    /// Database + vault overview.
    Stats,
    /// 本地自检（schema/FTS/vault 抽验/对象缺失/快照新鲜度），只读不改数据。
    Doctor,
    /// Run the MCP server on stdio.
    Mcp,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Read an excerpt of one session (includes lineage).
    Show {
        session_id: String,
        #[arg(long, default_value = "60")]
        max: u32,
        /// 聚焦到某一行（来源指针跳转）：截取该行及之前的尾部窗口。
        #[arg(long)]
        line: Option<i64>,
    },
    /// 导出压缩点之前的对话（压缩前备份，0.3.8）。默认 JSON；--markdown 出
    /// 可读文本，--out 写文件。会话未压缩过时 = 全量导出（compact_line_no 为空）。
    Precompact {
        session_id: String,
        #[arg(long)]
        markdown: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 软删进回收站：搜索/详情/列表全部隐藏，`session restore` 可恢复。
    Delete { session_id: String },
    /// 从回收站恢复（清 deleted_at，恢复后原样可搜）。
    Restore { session_id: String },
    /// 列出回收站（全部软删会话）。
    Trash,
    /// 物理清除一个回收站会话（默认超 30 天保留期；--force 强制，备份归档不变）。
    Purge {
        session_id: String,
        /// 跳过 stdin 确认（预览仍会打印）。
        #[arg(long)]
        yes: bool,
        /// 保留期内强制清除（知情门控由本确认承担）。
        #[arg(long)]
        force: bool,
        /// 保留离线档案（1.0.0 起默认移除——彻底删除即彻底）。
        #[arg(long)]
        keep_archive: bool,
    },
    /// 清空回收站里所有超期会话（物理删除，同 Purge 门控）。
    Empty {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    Add {
        #[arg(long)]
        r#type: String,
        #[arg(long)]
        content: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "project")]
        scope: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        source_session: Option<String>,
        #[arg(long)]
        source_message: Option<i64>,
    },
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        r#type: Option<String>,
        #[arg(long)]
        status: Option<String>,
        /// 按来源会话的 agent 过滤（无来源指针的记忆会被滤掉）。
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    Search {
        query: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        r#type: Option<String>,
        #[arg(long)]
        status: Option<String>,
        /// 按来源会话的 agent 过滤。
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    Confirm { id: String },
    Archive { id: String },
    Supersede {
        id: String,
        #[arg(long)]
        by: String,
    },
}

#[derive(Subcommand)]
enum IndexCmd {
    /// 索引模式与体量状态（模式 / 工具输出行数 / 库与空闲页体积）。
    Status,
    /// 开启工具输出全文索引：重建索引（大库分钟级），数据库体积明显增大。
    EnableTools,
    /// 关闭工具输出索引并回收磁盘空间（含 VACUUM，分钟级）。数据不动，可随时重开。
    DisableTools,
    /// 整理数据库空闲页（VACUUM）。索引切换 / 大量删除后使用。
    Compact,
}

#[derive(Subcommand)]
enum AgentsCmd {
    /// 检测五源 + 主流 agent 观察名单：默认根是否存在、各 agent 会话数、
    /// 已登记的自定义根、停用状态。
    List,
    /// 登记一个自定义采集根（仅文件型 agent：claude/codex/zcode/kimi）。
    Add {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        path: PathBuf,
    },
    /// 移除一个自定义采集根（不影响已导入的数据）。
    Remove {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        path: PathBuf,
    },
    /// 停用一个内置 agent（不再采集其源；已入库历史保留可搜）。
    /// 用于卸载了某个 agent 的机器（如已删 opencode）。
    Disable { agent: String },
    /// 重新启用一个已停用的 agent。
    Enable { agent: String },
}

#[derive(Subcommand)]
enum ProjectsCmd {
    /// 手动登记一个项目文件夹（只建项目行，不涉及采集；路径需真实存在）。
    Add { path: PathBuf },
    /// 废弃一个项目（软归档：列表全端隐藏、资产保留、可恢复、导入不复活）。
    Archive { project: String },
    /// 恢复一个已废弃的项目。
    Restore { project: String },
}

#[derive(Subcommand)]
enum MemoryFilesCmd {
    /// List monitored native memory files with revision counts.
    List,
    /// Show one file's metadata, revision timeline and content.
    Show {
        id: i64,
        /// Revision id (from the timeline); default: latest.
        #[arg(long)]
        revision: Option<i64>,
    },
    /// Diff the last N revisions (consecutive pairs, oldest -> newest).
    Diff {
        id: i64,
        #[arg(long, default_value = "2")]
        last: usize,
    },
    /// Search latest revisions of all monitored memory files.
    Search {
        query: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum HandoffCmd {
    Create {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "")]
        title: String,
        #[arg(long, default_value = "")]
        done: String,
        #[arg(long, default_value = "")]
        state: String,
        #[arg(long, default_value = "")]
        decisions: String,
        #[arg(long, default_value = "")]
        files: String,
        #[arg(long, default_value = "")]
        issues: String,
        #[arg(long, default_value = "")]
        next: String,
        #[arg(long)]
        session: Option<String>,
    },
    Show {
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum BundleCmd {
    /// Create a bundle (.tar.gz): DB snapshot + referenced CAS objects.
    Create {
        /// Output path, e.g. ~/backups/youmem-2026.tar.gz
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Verify a bundle: re-hash every object, reconcile with the manifest.
    Verify { path: PathBuf },
    /// Restore a bundle. Default: fresh --home. --merge: merge into existing library.
    Restore {
        path: PathBuf,
        /// Target home (default: $YOUMEM_HOME or ~/.yourmem).
        #[arg(long)]
        home: Option<PathBuf>,
        #[arg(long)]
        merge: bool,
    },
}

#[derive(Subcommand)]
enum BackupCmd {
    Status,
    /// Rebuild a raw session JSONL from the vault.
    Export {
        session_id: String,
        out: PathBuf,
    },
    /// Consistent database snapshot (VACUUM INTO), with retention.
    Db {
        #[arg(long, default_value = "10")]
        keep: usize,
    },
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).expect("json"));
}

fn run_import(
    home: &Path,
    claude_dir: Option<PathBuf>,
    codex_dir: Option<PathBuf>,
    zcode_dir: Option<PathBuf>,
    kimi_dir: Option<PathBuf>,
    opencode_db: Option<PathBuf>,
    hermes_db: Option<PathBuf>,
) -> Result<ingest::ImportOutcome> {
    // 根解析优先级：CLI 旗标 > YOUMEM_*_DIR 环境变量 > 默认（与 watch/MCP
    // 的 import_defaults 同一套环境变量——此前 CLI import 只认旗标，环境变量
    // 在 CLI/后台两条路径行为不一致，测试冒烟三次踩坑）
    let pick = |flag: Option<PathBuf>, var: &str, d: fn() -> PathBuf| {
        flag.or_else(|| std::env::var_os(var).map(PathBuf::from)).unwrap_or_else(d)
    };
    let roots = vec![
        (adapters::AGENT_CLAUDE, pick(claude_dir, "YOUMEM_CLAUDE_DIR", yourmem::default_claude_root)),
        (adapters::AGENT_CODEX, pick(codex_dir, "YOUMEM_CODEX_DIR", yourmem::default_codex_root)),
        (adapters::AGENT_ZCODE, pick(zcode_dir, "YOUMEM_ZCODE_DIR", yourmem::default_zcode_root)),
        (adapters::AGENT_KIMI, pick(kimi_dir, "YOUMEM_KIMI_DIR", yourmem::default_kimi_root)),
    ];
    let oc_explicit = opencode_db
        .or_else(|| std::env::var_os("YOUMEM_OPENCODE_DB").map(PathBuf::from));
    // 显式指定的库不存在必须报错：默认路径缺失=未装 opencode 是正常态；但
    // 旗标/环境变量指向的文件多半是手误，静默跳过会让人以为采到了
    if let Some(p) = &oc_explicit {
        anyhow::ensure!(p.is_file(), "显式指定的 OpenCode 库不存在: {}", p.display());
    }
    let oc_db = oc_explicit.unwrap_or_else(adapters::opencode::default_db_path);
    let hm_explicit = hermes_db.or_else(|| std::env::var_os("YOUMEM_HERMES_DB").map(PathBuf::from));
    if let Some(p) = &hm_explicit {
        anyhow::ensure!(p.is_file(), "显式指定的 Hermes 库不存在: {}", p.display());
    }
    let hm_db = hm_explicit.unwrap_or_else(adapters::hermes::default_db_path);
    let mut roots: Vec<(String, PathBuf)> = roots.into_iter().map(|(a, p)| (a.to_string(), p)).collect();
    // extra_roots 合并 + 停用过滤（与 import_defaults 同语义；此前 CLI 漏掉，
    // agents add 登记的自定义根只有桌面 app/MCP 能采到）
    yourmem::ingest::apply_extra_roots_and_gates(home, &mut roots);
    let refs: Vec<(&str, PathBuf)> = roots.iter().map(|(a, p)| (a.as_str(), p.clone())).collect();
    ingest::import_with(home, &refs, oc_db.is_file().then_some(oc_db.as_path()), hm_db.is_file().then_some(hm_db.as_path()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = data_home();
    std::fs::create_dir_all(&home)?;

    // 本地使用侧指标（DESIGN-0.3 §11）：仅命令名+时间戳，不外传；失败静默。
    let cmd_name = match &cli.cmd {
        Cmd::Import { .. } => "import",
        Cmd::Watch { .. } => "watch",
        Cmd::Search { .. } => "search",
        Cmd::Projects { .. } => "projects",
        Cmd::Context { .. } => "context",
        Cmd::Session { cmd } => match cmd {
            SessionCmd::Show { .. } => "session show",
            SessionCmd::Precompact { .. } => "session precompact",
            SessionCmd::Delete { .. } => "session delete",
            SessionCmd::Restore { .. } => "session restore",
            SessionCmd::Trash => "session trash",
            SessionCmd::Purge { .. } => "session purge",
            SessionCmd::Empty { .. } => "trash empty",
        },
        Cmd::Dossier { .. } => "dossier",
        Cmd::Digest { .. } => "digest",
        Cmd::Memory { .. } => "memory",
        Cmd::MemoryFiles { .. } => "memory-files",
        Cmd::Artifacts { .. } => "artifacts",
        Cmd::Handoff { .. } => "handoff",
        Cmd::Backup { .. } => "backup",
        Cmd::Bundle { .. } => "bundle",
        Cmd::RestoreAgents { .. } => "restore-agents",
        Cmd::Setup { .. } => "setup",
        Cmd::Agents { .. } => "agents",
        Cmd::Index { cmd } => match cmd {
            IndexCmd::Status => "index status",
            IndexCmd::EnableTools => "index enable-tools",
            IndexCmd::DisableTools => "index disable-tools",
            IndexCmd::Compact => "index compact",
        },
        Cmd::Stats => "stats",
        Cmd::Doctor => "doctor",
        Cmd::Mcp => "mcp",
    };
    if !matches!(cli.cmd, Cmd::Mcp | Cmd::Bundle { cmd: BundleCmd::Restore { .. } }) {
        if let Ok(conn) = db::open(&home) {
            let _ = db::log_usage(&conn, "cli", cmd_name);
        }
    }

    match cli.cmd {
        Cmd::Mcp => return yourmem::mcp::serve(&home),

        Cmd::Import { claude_dir, codex_dir, zcode_dir, kimi_dir, opencode_db, hermes_db } => {
            let outcome = run_import(&home, claude_dir, codex_dir, zcode_dir, kimi_dir, opencode_db, hermes_db)?;
            print_json(&ingest::outcome_json(&outcome));
        }

        Cmd::Watch { interval } => {
            eprintln!("yourmem watch: importing every {interval}s (ctrl-c to stop)");
            loop {
                // 单轮失败不退出（持续采集定位）：最典型场景是 import_with 的锁
                // fail-closed 30s 超时——另一个命令在等 stdin 确认时 watch 不该死
                match run_import(&home, None, None, None, None, None, None) {
                    Ok(outcome) if outcome.messages_added > 0
                        || outcome.lines_archived > 0
                        || outcome.memory_revisions_added > 0 =>
                    {
                        print_json(&ingest::outcome_json(&outcome));
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("yourmem watch: import 失败（下轮重试）: {e:#}"),
                }
                std::thread::sleep(std::time::Duration::from_secs(interval));
            }
        }

        Cmd::Search { query, project, agent, kind, limit } => {
            let result = db::open(&home).and_then(|conn| db::search(&conn, &db::SearchOpts { query, project, agent, kind, limit }));
            yourmem::recall_status::record(&home, "cli", "search", result.is_ok(), result.as_ref().ok().map(|v| v.len() as u64));
            let hits = result?;
            print_json(&json!({ "results": hits }));
        }

        Cmd::Projects { cmd } => {
            let conn = db::open(&home)?;
            match cmd {
                None => print_json(&json!({
                    "projects": db::list_projects(&conn)?,
                    "archived": db::list_archived_projects(&conn)?,
                })),
                Some(ProjectsCmd::Add { path }) => {
                    let p = expand_home(&path.to_string_lossy());
                    anyhow::ensure!(Path::new(&p).is_dir(), "directory not found: {p}");
                    let (id, created) = db::add_project(&conn, &p)?;
                    print_json(&json!({ "id": id, "created": created, "path": p }))
                }
                Some(ProjectsCmd::Archive { project }) => {
                    let (id, name, _) = db::resolve_project(&conn, Some(&project), None)?
                        .ok_or_else(|| anyhow::anyhow!("no matching project: {project}"))?;
                    db::set_project_archived(&conn, id, true)?;
                    print_json(&json!({ "id": id, "name": name, "archived": true }))
                }
                Some(ProjectsCmd::Restore { project }) => {
                    let (id, name, _) = db::resolve_project(&conn, Some(&project), None)?
                        .ok_or_else(|| anyhow::anyhow!("no matching project: {project}"))?;
                    db::set_project_archived(&conn, id, false)?;
                    print_json(&json!({ "id": id, "name": name, "archived": false }))
                }
            }
        }

        Cmd::Context { project, markdown } => {
            let conn = db::open(&home)?;
            let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
            let (pid, ..) = db::resolve_project(&conn, project.as_deref(), cwd.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("no matching project; run `yourmem import` first"))?;
            let mut d = db::project_context(&conn, pid)?;
            d["source_review"] = yourmem::project_review::status(&conn, &home, pid)?;
            if markdown {
                print!("{}", yourmem::dossier::render_context_markdown(&d));
            } else {
                print_json(&d);
            }
        }

        Cmd::Dossier { project, markdown, out } => {
            let conn = db::open(&home)?;
            let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
            let (pid, ..) = db::resolve_project(&conn, project.as_deref(), cwd.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("no matching project; run `yourmem import` first"))?;
            let d = yourmem::dossier::project_dossier(&conn, pid)?;
            if markdown || out.is_some() {
                let md = yourmem::dossier::render_markdown(&d);
                match out {
                    Some(path) => std::fs::write(&path, md)?,
                    None => print!("{md}"),
                }
            } else {
                print_json(&d);
            }
        }

        Cmd::Digest { day, markdown } => {
            let conn = db::open(&home)?;
            let day = day.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
            let d = yourmem::dossier::daily_digest(&conn, &day)?;
            if markdown {
                print!("{}", yourmem::dossier::render_digest_markdown(&d));
            } else {
                print_json(&d);
            }
        }

        Cmd::Session { cmd } => {
            let mut conn = db::open(&home)?;
            match cmd {
                SessionCmd::Show { session_id, max, line } => {
                    print_json(&db::read_session(&conn, &session_id, max, line, false)?)
                }
                SessionCmd::Precompact { session_id, markdown, out } => {
                    let d = db::read_session(&conn, &session_id, u32::MAX, None, true)?;
                    if markdown || out.is_some() {
                        let md = yourmem::dossier::render_session_markdown(&d);
                        match out {
                            Some(path) => std::fs::write(&path, md)?,
                            None => print!("{md}"),
                        }
                    } else {
                        print_json(&d);
                    }
                }
                SessionCmd::Delete { session_id } => {
                    db::set_session_deleted(&conn, &session_id, true)?;
                    print_json(&json!({ "session_id": session_id, "deleted": true }));
                }
                SessionCmd::Restore { session_id } => {
                    db::set_session_deleted(&conn, &session_id, false)?;
                    print_json(&json!({ "session_id": session_id, "restored": true }));
                }
                SessionCmd::Purge { session_id, yes, force, keep_archive } => {
                    let r = yourmem::trash::purge_cli(&mut conn, &home, &session_id, force, keep_archive, |plan| {
                        print_json(plan);
                        if plan["can_purge"] != true && !force {
                            anyhow::bail!("不满足物理清除条件（见上方预览；保留期内可加 --force 强制）");
                        }
                        if !yes {
                            eprint!("确认物理删除？将先归档到 backups/purge/。输入 y 继续: ");
                            let mut line = String::new();
                            std::io::stdin().read_line(&mut line)?;
                            if line.trim() != "y" { anyhow::bail!("已取消"); }
                        }
                        Ok(())
                    })?;
                    print_json(&r);
                }
                SessionCmd::Empty { yes } => {
                    let r = yourmem::trash::empty_cli(&mut conn, &home, |plan| {
                        print_json(plan);
                        if !yes {
                            let n = plan["overdue_sessions"].as_array().map(Vec::len).unwrap_or(0);
                            eprint!("确认物理删除以上 {} 个超期会话（归档 {} 个对象 {} 字节）？输入 y 继续: ",
                                n, plan["exclusive_objects"], plan["exclusive_bytes"]);
                            let mut line = String::new();
                            std::io::stdin().read_line(&mut line)?;
                            if line.trim() != "y" { anyhow::bail!("已取消"); }
                        }
                        Ok(())
                    })?;
                    print_json(&r);
                }
                SessionCmd::Trash => {
                    print_json(&json!({ "trash": db::trash_sessions(&conn)? }));
                }
            }
        }

        Cmd::Memory { cmd } => {
            let conn = db::open(&home)?;
            let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
            match cmd {
                MemoryCmd::Add { r#type, content, project, scope, status, source_session, source_message } => {
                    let pid = if scope == "global" {
                        None
                    } else {
                        Some(
                            db::resolve_project(&conn, project.as_deref(), cwd.as_deref())?
                                .ok_or_else(|| anyhow::anyhow!("no matching project"))?
                                .0,
                        )
                    };
                    // 写路径纪律（engramory 吸收）：查重在 save 之前——知情门控，不拦截
                    let (id, similar) = db::save_memory_with_similar(&conn, &db::MemoryInput {
                        project_id: pid,
                        scope: &scope,
                        r#type: &r#type,
                        content: &content,
                        status: status.as_deref(),
                        source_session_id: source_session.as_deref(),
                        source_message_id: source_message,
                    })?;
                    if !similar.is_empty() {
                        eprintln!("提示：疑似重复（纪律：更新优于复制，考虑 update_memory 取代而非新增）：");
                        for m in &similar {
                            eprintln!("  [{}] {}…（id {}，重合 {}）",
                                m["status"].as_str().unwrap_or("?"),
                                m["content"].as_str().unwrap_or("").chars().take(40).collect::<String>(),
                                m["id"].as_str().unwrap_or("?"), m["shared_trigrams"]);
                        }
                    }
                    print_json(&json!({ "memory_id": id, "similar_count": similar.len() }));
                }
                MemoryCmd::List { project, scope, r#type, status, agent, limit } => {
                    let pid = match &project {
                        Some(p) => Some(db::resolve_project(&conn, Some(p), cwd.as_deref())?
                            .ok_or_else(|| anyhow::anyhow!("no matching project"))?.0),
                        None => None,
                    };
                    let mems = db::list_memories(&conn, &db::MemoryFilter {
                        project_id: pid, scope, r#type, status, agent, include_global: pid.is_some(), limit,
                    })?;
                    print_json(&json!({ "memories": mems }));
                }
                MemoryCmd::Search { query, project, r#type, status, agent, limit } => {
                    let pid = match &project {
                        Some(p) => Some(db::resolve_project(&conn, Some(p), cwd.as_deref())?
                            .ok_or_else(|| anyhow::anyhow!("no matching project"))?.0),
                        None => None,
                    };
                    let mems = db::search_memory(&conn, &query, &db::MemoryFilter {
                        project_id: pid, scope: None, r#type, status, agent, include_global: pid.is_some(), limit,
                    })?;
                    print_json(&json!({ "memories": mems }));
                }
                MemoryCmd::Confirm { id } => {
                    db::update_memory_status(&conn, &id, "confirm", None)?;
                    print_json(&json!({ "memory_id": id, "status": "confirmed" }));
                }
                MemoryCmd::Archive { id } => {
                    db::update_memory_status(&conn, &id, "archive", None)?;
                    print_json(&json!({ "memory_id": id, "status": "archived" }));
                }
                MemoryCmd::Supersede { id, by } => {
                    db::update_memory_status(&conn, &id, "supersede", Some(&by))?;
                    print_json(&json!({ "memory_id": id, "status": "superseded", "superseded_by": by }));
                }
            }
        }

        Cmd::MemoryFiles { cmd } => {
            let conn = db::open(&home)?;
            match cmd {
                MemoryFilesCmd::List => {
                    print_json(&json!({ "memory_files": db::list_memory_files(&conn)? }));
                }
                MemoryFilesCmd::Show { id, revision } => {
                    print_json(&yourmem::memfiles::show(&conn, &home, id, revision)?);
                }
                MemoryFilesCmd::Diff { id, last } => {
                    print_json(&yourmem::memfiles::diff(&conn, &home, id, last)?);
                }
                MemoryFilesCmd::Search { query, limit } => {
                    let hits = yourmem::memfiles::search(&conn, &home, &query, limit)?;
                    print_json(&json!({ "results": hits }));
                }
            }
        }

        Cmd::Artifacts { project, session, limit } => {
            let conn = db::open(&home)?;
            let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
            let pid = match &project {
                Some(p) => Some(db::resolve_project(&conn, Some(p), cwd.as_deref())?
                    .ok_or_else(|| anyhow::anyhow!("no matching project"))?.0),
                None => None,
            };
            print_json(&json!({ "artifacts": db::list_artifacts(&conn, pid, session.as_deref(), limit)? }));
        }

        Cmd::Handoff { cmd } => {
            let conn = db::open(&home)?;
            let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
            match cmd {
                HandoffCmd::Create { project, title, done, state, decisions, files, issues, next, session } => {
                    let (pid, name, _) = db::resolve_project(&conn, project.as_deref(), cwd.as_deref())?
                        .ok_or_else(|| anyhow::anyhow!("no matching project"))?;
                    let fields = db::HandoffFields {
                        title: &title, done: &done, state: &state, decisions: &decisions,
                        files_changed: &files, open_issues: &issues, next_steps: &next,
                        session_id: session.as_deref(),
                    };
                    let id = db::create_handoff(&conn, pid, &fields)?;
                    print_json(&json!({ "handoff_id": id, "project": name }));
                }
                HandoffCmd::Show { project } => {
                    let (pid, ..) = db::resolve_project(&conn, project.as_deref(), cwd.as_deref())?
                        .ok_or_else(|| anyhow::anyhow!("no matching project"))?;
                    print_json(&json!({ "latest_handoff": db::latest_handoff(&conn, pid)? }));
                }
            }
        }

        Cmd::Backup { cmd } => {
            let conn = db::open(&home)?;
            match cmd {
                BackupCmd::Status => print_json(&vault::status(&conn, &home)?),
                BackupCmd::Export { session_id, out } => {
                    let lines = vault::export_session(&conn, &home, &session_id, &out)?;
                    print_json(&json!({ "exported_lines": lines, "out": out }));
                }
                BackupCmd::Db { keep } => {
                    let result = vault::snapshot_db(&conn, &home, keep)?;
                    print_json(&result);
                }
            }
        }

        Cmd::Bundle { cmd } => match cmd {
            BundleCmd::Create { out, agent, project } => {
                let conn = db::open(&home)?;
                let filter = yourmem::bundle::BundleFilter { agent, project };
                print_json(&yourmem::bundle::create(&conn, &home, &out, &filter)?);
            }
            BundleCmd::Verify { path } => {
                print_json(&yourmem::bundle::verify(&path)?);
            }
            BundleCmd::Restore { path, home: target, merge } => {
                let target = target.unwrap_or_else(|| home.clone());
                let restored = yourmem::bundle::restore(&path, &target, merge)?;
                if let Ok(conn) = db::open(&target) {
                    let _ = db::log_usage(&conn, "cli", "bundle");
                }
                print_json(&restored);
            }
        },

        Cmd::RestoreAgents { agent, session, force, yes } => {
            let conn = db::open(&home)?;
            // 门控 1/2：预览 + 确认（门控 3/4 在 execute 里：.bak 备份、字节级重建）
            let plan = yourmem::restore::plan(&conn, &agent, &session)?;
            print_json(&plan);
            if !yes {
                eprint!("确认执行写回？这是一次性迁移，不是同步。输入 y 继续: ");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if line.trim() != "y" {
                    anyhow::bail!("已取消");
                }
            }
            let result = yourmem::restore::execute(&conn, &home, &agent, &session, force)?;
            print_json(&result);
        }

        Cmd::Stats => {
            let conn = db::open(&home)?;
            print_json(&db::stats(&conn)?);
        }
        Cmd::Doctor => {
            let conn = db::open(&home)?;
            print_json(&doctor::run(&conn, &home)?);
        }

        Cmd::Setup { yes } => {
            let targets = yourmem::setup::Targets::default();
            let plan = yourmem::setup::plan(&targets)?;
            print_json(&plan);
            if !yes {
                eprint!("确认执行接入？覆盖前自动 .bak 备份，重复执行幂等。输入 y 继续: ");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if line.trim() != "y" {
                    anyhow::bail!("已取消");
                }
            }
            print_json(&yourmem::setup::execute(&targets)?);
        }

        Cmd::Agents { cmd } => match cmd {
            AgentsCmd::List => {
                let conn = db::open(&home)?;
                print_json(&ingest::agent_sources(&home, &conn)?);
            }
            AgentsCmd::Add { agent, path } => {
                print_json(&ingest::add_extra_root(&home, &agent, &path)?);
            }
            AgentsCmd::Remove { agent, path } => {
                print_json(&ingest::remove_extra_root(&home, &agent, &path)?);
            }
            AgentsCmd::Disable { agent } => {
                let conn = db::open(&home)?;
                let _ = db::log_usage(&conn, "cli", "agents disable");
                print_json(&ingest::set_agent_disabled(&home, &agent, true)?);
            }
            AgentsCmd::Enable { agent } => {
                print_json(&ingest::set_agent_disabled(&home, &agent, false)?);
            }
        },

        Cmd::Index { cmd } => match cmd {
            IndexCmd::Status => {
                let conn = db::open(&home)?;
                print_json(&db::index_status(&conn)?);
            }
            IndexCmd::EnableTools => {
                let conn = db::open(&home)?;
                let _ = db::log_usage(&conn, "cli", "index enable-tools");
                print_json(&db::set_tool_index(&home, &conn, true)?);
            }
            IndexCmd::DisableTools => {
                let conn = db::open(&home)?;
                let _ = db::log_usage(&conn, "cli", "index disable-tools");
                print_json(&db::set_tool_index(&home, &conn, false)?);
            }
            IndexCmd::Compact => {
                let conn = db::open(&home)?;
                let before = db::index_status(&conn)?;
                let _ = db::log_usage(&conn, "cli", "index compact");
                conn.execute_batch("VACUUM")?;
                let mut after = db::index_status(&conn)?;
                after["before_bytes"] = before["db_bytes"].clone();
                print_json(&after);
            }
        },
    }
    Ok(())
}
