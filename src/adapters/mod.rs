//! Source adapters: one per agent format.
//!
//! Capability contract (per DESIGN.md): `parse` is mandatory. Adapters must
//! be defensive — unknown line types and schema drift are skipped, never
//! fatal. Session identity is derived from the file name so incremental
//! re-imports stay consistent without parser state.

pub mod claude;
pub mod codex;
pub mod hermes;
pub mod kimi;
pub mod opencode;
pub mod zcode;

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::models::ParseOutput;

pub const AGENT_CLAUDE: &str = "claude";
pub const AGENT_CODEX: &str = "codex";
pub const AGENT_ZCODE: &str = "zcode";
pub const AGENT_KIMI: &str = "kimi";
/// hermes：多端网关 agent（~/.hermes/state.db，SQLite 单库源，0.3.9 起）。
/// cron 来源按 hermes 本人裁定不采（运行日志，非知识资产）。
pub const AGENT_HERMES: &str = "hermes";

pub fn supported_agents() -> [&'static str; 4] {
    [AGENT_CLAUDE, AGENT_CODEX, AGENT_ZCODE, AGENT_KIMI]
}

/// Recursively find `*.jsonl` session files under a source root.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    out.sort();
    out
}

/// Native session id from the file name:
/// - claude: `<uuid>.jsonl` -> the stem
/// - codex: `rollout-<ts>-<uuid>.jsonl` -> trailing uuid when present
pub fn native_id(agent: &str, path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if agent == AGENT_ZCODE {
        // 文件名 model-io-sess_<uuid>.jsonl → 与 zcode 库里的会话 id 对齐
        if let Some(rest) = stem.strip_prefix("model-io-") {
            return rest.to_string();
        }
    }
    if agent == AGENT_KIMI {
        // 路径结构 …/sessions/wd_<hash>/session_<uuid>/agents/<agent>/wire.jsonl：
        // 身份从路径派生——main 即本体，子 agent 加 #<名字> 后缀独立成会话。
        // 锚定必须校验下一级是 agents：只认 session_ 前缀的话，祖先目录恰好
        // 叫 session_xxx 时所有 kimi 会话会折叠成同一 id 互相覆盖（自检 A6）。
        let comps: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        let mut anchor: Option<usize> = None;
        for i in 0..comps.len() {
            if comps[i].starts_with("session_")
                && comps.get(i + 1).map_or(false, |c| c == "agents")
            {
                anchor = Some(i); // 取最深的一个（wire.jsonl 的直接会话祖先）
            }
        }
        if let Some(pos) = anchor {
            let mut id = comps[pos].clone();
            if let Some(agent_name) = comps.get(pos + 2) {
                if agent_name != "main" {
                    id.push('#');
                    id.push_str(agent_name);
                }
            }
            return id;
        }
        // 退化兜底：无 session_/agents 结构（非标准布局）——父目录+文件名保证唯一
        let parent = path.parent().and_then(Path::file_name).map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        return format!("{parent}/{stem}");
    }
    if agent == AGENT_CODEX && stem.len() > 36 {
        // 用 get() 避免多字节字符落在切片边界上时 panic（防御式解析，永不 panic）。
        if let Some(tail) = stem.get(stem.len() - 36..) {
            if tail.chars().filter(|c| *c == '-').count() == 4 {
                return tail.to_string();
            }
        }
    }
    stem
}

pub fn session_key(agent: &str, native_id: &str) -> String {
    format!("{agent}:{native_id}")
}

/// Command that resumes this session in the native agent CLI (DESIGN-0.3 §4).
/// Displayed for the user to copy — yourmem never executes it (数据源只读)。
/// 产品裁定（2026-08-23）：保持 Coffee CLI 式的简单——一条裸命令，用户在自己
/// 项目目录里执行；不做 cd 拼接之类的自动化。
/// 实测备注：claude `--resume` 按项目目录查找会话（在别的 cwd 下执行会报
/// "No conversation found"），codex `resume` 全局查找，opencode `-s` 从启动目录恢复。
pub fn resume_command(agent: &str, native_id: &str) -> Option<String> {
    match agent {
        AGENT_CLAUDE => Some(format!("claude --resume {native_id}")),
        AGENT_CODEX => Some(format!("codex resume {native_id}")),
        opencode::AGENT_OPENCODE => Some(format!("opencode -s {native_id}")),
        // kimi -S 真机验证（2026-08-23 流水线派工即此命令）；子 agent 会话
        // （id 含 #）无独立恢复入口，不提供
        AGENT_KIMI if !native_id.contains('#') => Some(format!("kimi -S {native_id}")),
        // hermes 本人验证（2026-08-30）：`hermes --resume SESSION_ID` / `--continue`；
        // 会话 id 是 state.db 持久主键。cron 会话不采，无恢复语义问题。
        AGENT_HERMES => Some(format!("hermes --resume {native_id}")),
        _ => None,
    }
}

/// Parse a chunk of raw lines into normalized messages + session metadata.
/// `lines` are 1-based line numbers matching the vault manifest.
pub fn parse_chunk(agent: &str, lines: &[(u64, String)]) -> ParseOutput {
    match agent {
        AGENT_CLAUDE => claude::parse_lines(lines),
        AGENT_CODEX => codex::parse_lines(lines, false),
        AGENT_ZCODE => zcode::parse_lines(lines),
        AGENT_KIMI => kimi::parse_lines(lines),
        _ => ParseOutput::default(),
    }
}

/// 同 parse_chunk，但携带跨 chunk 状态：codex 的 event_msg 抑制需要知道该文件
/// 早前 chunk 是否出现过 response_item（ingest 从 source_files.saw_response_item
/// 读出）。其他 agent 忽略该参数。
pub fn parse_chunk_with_prior(agent: &str, lines: &[(u64, String)], prior: bool) -> ParseOutput {
    match agent {
        AGENT_CODEX => codex::parse_lines(lines, prior),
        _ => parse_chunk(agent, lines),
    }
}

/// 能力矩阵（UI-DESIGN §8.4 / Swob 式诚实自报）：六格——transcript/search/
/// lineage/resume/writeback/源加密。取值 "yes"|"partial"|"no"，每格注释即证据。
/// 新增 adapter 时必须更新此表（AGENTS.md 同步义务）。
pub fn capability_matrix() -> serde_json::Value {
    serde_json::json!([
        { "agent": AGENT_CLAUDE, "transcript": "yes", "search": "yes",
          // fork/compact 谱系边（session_uuids 目击表）
          "lineage": "yes", "resume": "yes",
          "writeback": "yes", "encrypted": "no",
          "notes": "压缩=新文件+compact 谱系边；写回限文件型（restore-agents）" },
        { "agent": AGENT_CODEX, "transcript": "yes", "search": "yes",
          "lineage": "no", "resume": "yes",
          "writeback": "yes", "encrypted": "no",
          "notes": "谱系/压缩点留接口（按样本门禁）；写回限文件型" },
        { "agent": opencode::AGENT_OPENCODE, "transcript": "yes", "search": "yes",
          "lineage": "no", "resume": "yes",
          // SQLite 单库源：只读原则，不向别人的库插行（restore.rs 注释）
          "writeback": "no", "encrypted": "no",
          "notes": "SQLite 单库源，写回不做；恢复走 bundle restore 进 yourmem" },
        { "agent": AGENT_ZCODE, "transcript": "yes", "search": "yes",
          "lineage": "no", "resume": "no",
          "writeback": "no", "encrypted": "no",
          "notes": "0.3.8 压缩点备份首发源；无原生 resume 入口；cwd 从元库补查" },
        { "agent": AGENT_KIMI, "transcript": "yes", "search": "yes",
          // 子 agent 以 #名字 独立成会话，是拆分不是谱系边
          "lineage": "no", "resume": "partial",
          "writeback": "no", "encrypted": "no",
          "notes": "resume 仅 main 会话（kimi -S）；子 agent 无独立恢复入口" },
        { "agent": AGENT_HERMES, "transcript": "yes", "search": "yes",
          "lineage": "yes", "resume": "yes",
          "writeback": "no", "encrypted": "no",
          "notes": "SQLite 单库源；cron 来源按本人裁定不采；压缩点检测内置待首例校准" },
    ])
}

/// 观察名单里的加密源（DESIGN-0.3 §3.2 硬阻断）：能力矩阵如实标注，不做。
pub fn encrypted_watchlist() -> serde_json::Value {
    serde_json::json!([
        { "agent": "trae", "encrypted": "yes", "notes": "ModularData 加密，硬阻断不做" },
        { "agent": "antigravity", "encrypted": "yes", "notes": "加密 protobuf，硬阻断不做" },
    ])
}
