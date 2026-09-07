//! `yourmem setup`：一键接入本机 agent（DESIGN-0.3 §9，scripts/setup-agents.md
//! 认知的产品化）。检测 agent → 注册 MCP + 写全局指令。
//! 一切写操作走"预览 → 确认 → .bak 备份 → 执行"门控（同 §5.2 模式）；
//! 全部操作幂等，重复执行安全。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// 全局指令片段（与 scripts/setup-agents.md 保持一致——改这里同步改那里）。
/// 这段解决的不是"能用"，是"知道什么时候该用"（codex exec 实测验证）。
pub const INSTRUCTION_BLOCK: &str = r#"
## yourmem 跨 agent 记忆库（MCP）

当用户问到「以前是不是做过」「之前怎么做的」「某项目的历史/交接」「那个文件是谁生成的」时，
**先调 yourmem 工具再回答**，不要只凭当前仓库或当前会话下结论。

- `search_history`：搜所有 agent（Codex/Claude/OpenCode/ZCode/Kimi/Hermes）的历史会话，命中后用 `read_session` 钻取
- `get_project_context` / `get_recent_work`：项目背景、已确认记忆、最新 handoff、待办
- `read_native_memory`：查你自己 memory 文件（MEMORY.md / AGENTS.md）的历史版本与修订
- `save_memory`：重要决定/规则/待办主动记录（decision/rule 会进入待确认队列）；
  保存前注意结果里的 similar 提示——疑似重复时更新/取代旧条而非再存一条；
  lesson/preference 写清 Why 与 How to apply；git/代码/CLAUDE.md 已记录的不存
- `create_handoff`：一段工作收尾时写交接，注明 session_id

止损规则：检索连续两次无命中就停止，转当前仓库或直接询问用户，不要换词重试。
"#;

const INSTRUCTION_MARKER: &str = "yourmem 跨 agent 记忆库";

/// 指令块三态：全文在 → done；只有 marker（旧版块）→ stale；都没有 → todo。
/// marker 存在性检测会把内容升级过的机器永远挡在旧版（真机教训同 MCP 版本门
/// 19f7bc7），所以判 done 必须比当前全文。
fn instruction_status(content: &str) -> &'static str {
    if content.contains(INSTRUCTION_BLOCK.trim()) {
        "done"
    } else if content.contains(INSTRUCTION_MARKER) {
        "stale"
    } else {
        "todo"
    }
}

/// 所有可能被触及的路径（预览就是打印这份清单）。测试注入临时目录。
pub struct Targets {
    pub claude_json: PathBuf,   // ~/.claude.json（user scope MCP 注册表）
    pub claude_md: PathBuf,     // ~/.claude/CLAUDE.md
    pub codex_config: PathBuf,  // ~/.codex/config.toml
    pub codex_agents: PathBuf,  // ~/.codex/AGENTS.md
    pub zcode_config: PathBuf,  // ~/.zcode/cli/config.json（mcp.servers）
    pub kimi_mcp: PathBuf,      // ~/.kimi-code/mcp.json（mcpServers）
    pub gemini_settings: PathBuf, // ~/.gemini/settings.json（mcpServers）
    pub cursor_mcp: PathBuf,    // ~/.cursor/mcp.json（mcpServers）
    pub hermes_config: PathBuf, // ~/.hermes/config.yaml（mcp_servers 段，0.3.9 起）
}

impl Default for Targets {
    fn default() -> Self {
        let home = crate::home_dir();
        Self {
            claude_json: home.join(".claude.json"),
            claude_md: home.join(".claude").join("CLAUDE.md"),
            codex_config: home.join(".codex").join("config.toml"),
            codex_agents: home.join(".codex").join("AGENTS.md"),
            zcode_config: home.join(".zcode").join("cli").join("config.json"),
            kimi_mcp: home.join(".kimi-code").join("mcp.json"),
            gemini_settings: home.join(".gemini").join("settings.json"),
            cursor_mcp: home.join(".cursor").join("mcp.json"),
            hermes_config: home.join(".hermes").join("config.yaml"),
        }
    }
}

impl Targets {
    fn agent_present(&self, agent: &str) -> bool {
        match agent {
            "claude" => self.claude_md.parent().map(|p| p.is_dir()).unwrap_or(false) || self.claude_json.is_file(),
            "codex" => self.codex_config.parent().map(|p| p.is_dir()).unwrap_or(false),
            "zcode" => self.zcode_config.parent().map(|p| p.is_dir()).unwrap_or(false),
            "kimi" => self.kimi_mcp.parent().map(|p| p.is_dir()).unwrap_or(false),
            "gemini" => self.gemini_settings.parent().map(|p| p.is_dir()).unwrap_or(false),
            "cursor" => self.cursor_mcp.parent().map(|p| p.is_dir()).unwrap_or(false),
            "hermes" => self.hermes_config.is_file(),
            _ => false,
        }
    }

    /// 已注册的 MCP 命令路径。不同 agent 的配置位置与键名不同，逐一按其
    /// 真机格式读取，不能把“都是 JSON”误当成相同 schema。
    fn registered_mcp_command(&self, agent: &str) -> Option<String> {
        match agent {
            "claude" => json_registered_command(&self.claude_json, "/mcpServers/yourmem/command"),
            "zcode" => json_registered_command(&self.zcode_config, "/mcp/servers/yourmem/command"),
            "kimi" => json_registered_command(&self.kimi_mcp, "/mcpServers/yourmem/command"),
            "gemini" => json_registered_command(&self.gemini_settings, "/mcpServers/yourmem/command"),
            "cursor" => json_registered_command(&self.cursor_mcp, "/mcpServers/yourmem/command"),
            "hermes" => hermes_registered_command(&std::fs::read_to_string(&self.hermes_config).unwrap_or_default()),
            "codex" => codex_registered_command(&std::fs::read_to_string(&self.codex_config).unwrap_or_default()),
            _ => None,
        }
    }
}

fn json_registered_command(path: &Path, pointer: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.pointer(pointer).cloned())
        .and_then(|v| v.as_str().map(str::to_string))
}

/// 从 config.yaml 文本里找 mcp_servers 段下 `  yourmem:` 块的 command 值
/// （不引 YAML 解析器——hermes 的 config.yaml 有注释与手排结构，不能整体重写）。
/// 层级按缩进识别：段键在列 0，会话名在 2 空格，属性在 4 空格。
fn hermes_registered_command(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let ms = lines.iter().position(|l| {
        !l.trim_start().starts_with('#') && l.starts_with("mcp_servers:")
    })?;
    let mut in_block = false;
    for l in &lines[ms + 1..] {
        let indent = l.len() - l.trim_start().len();
        if l.trim().is_empty() {
            continue;
        }
        if indent <= 2 {
            // 回到段键或下一个同级键：yourmem 块结束
            if in_block {
                return None;
            }
            in_block = l.trim() == "yourmem:" && indent == 2;
            continue;
        }
        if in_block && indent >= 4 {
            if let Some((k, v)) = l.trim().split_once(':') {
                if k.trim() == "command" {
                    return Some(
                        v.split('#').next().unwrap_or("").trim().trim_matches('"').trim_matches('\'').to_string(),
                    );
                }
            }
        }
    }
    None
}

/// 表头识别容忍行尾注释：`[mcp_servers.yourmem] # 注释` 也是命中
///（自检 B4：严格等值匹配遇注释会判 todo → 追加重复表，TOML 直接非法）。
fn is_yourmem_table_header(t: &str) -> bool {
    t.split('#').next().unwrap_or("").trim() == "[mcp_servers.yourmem]"
}

/// 从 config.toml 文本里找 [mcp_servers.yourmem] 块的 command 值（不引 TOML 解析器）。
fn codex_registered_command(content: &str) -> Option<String> {
    let mut in_block = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_block = is_yourmem_table_header(t);
            continue;
        }
        if in_block {
            // 键名精确匹配：starts_with("command") 会误吞 command_timeout 等
            if let Some((k, v)) = t.split_once('=') {
                if k.trim() == "command" {
                    return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                }
            }
        }
    }
    None
}

/// 运行 `<cmd> --version` 取版本（yourmem 的 clap 输出形如 "yourmem 0.3.1"）。
/// 只探测用户自己配置文件里登记的命令，不执行任意其他东西。
/// 带超时强杀：登记的命令可能是 GUI 二进制（真机 2026-08-30：claude.json 曾
/// 注册过桌面 app，`--version` 会启动窗口且永不退出，无超时则 setup 永久卡死）。
fn registered_version(cmd: &str) -> Option<String> {
    let mut child = crate::background_command(cmd)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    use std::io::Read as _;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    out.lines().next()?.split_whitespace().nth(1).map(str::to_string)
}

/// MCP 注册状态：todo（未注册）/ done（已注册且是本版本）/ stale（注册的命令是旧版本
/// 或读不到版本——真机教训 2026-08-23：setup 只查"已注册"，把 0.1.0 旧二进制放过了，
/// agent 调 MCP 全部落空）。
fn mcp_status(cmd: Option<String>) -> (&'static str, Value) {
    match cmd {
        None => ("todo", Value::Null),
        Some(c) => match registered_version(&c) {
            Some(v) if v == env!("CARGO_PKG_VERSION") => {
                ("done", json!({ "registered": c, "version": v }))
            }
            other => (
                "stale",
                json!({
                    "registered": c,
                    "version": other.unwrap_or_else(|| "未知".into()),
                    "note": "已注册的命令与当前版本不一致，重新执行 setup 会更新注册",
                }),
            ),
        },
    }
}

/// 当前 exe 是否是 GUI app 本体：路径在 `.app/Contents/MacOS` 内，
/// 或文件名主干是 `yourmem-app`（Tauri debug 产物名；NSIS 安装包主程序名
/// 是 productName `yourmem.exe`，与 CLI 同名，无法靠文件名区分——见下）。
#[allow(dead_code)]
fn is_gui_bundle_exe(path: &Path) -> bool {
    path.file_stem().map(|n| n == "yourmem-app").unwrap_or(false)
        || path.to_string_lossy().contains(".app/Contents/MacOS")
}

/// 解析要注册进各 agent MCP 配置的二进制路径。
/// 0.4.6 起桌面 app 本体直接兼任 agent 端点（main.rs 参数路由：`mcp` 进
/// stdio 服务、`--version` 打印即退），所以任何场景下 current_exe 都是合法
/// 注册对象——0.4.5 之前"注册了 app 本体导致 GUI 挂起"的事故从根上消灭，
/// 文件名猜测（8-30 修复与同日 Windows file_stem 修补）整体退役。
/// dev 的 GUI 产物（yourmem-app.exe）同样具备 mcp/--version，行为一致。
pub fn resolve_cli_exe() -> Result<PathBuf> {
    std::env::current_exe().context("current_exe")
}

/// 门控要素 1：预览。返回每个 agent 的每个动作及其当前状态（done/todo/skip）。
const SETUP_AGENTS: [&str; 7] = ["claude", "codex", "zcode", "kimi", "gemini", "cursor", "hermes"];

/// 只预览用户选中的 agent。桌面端用它把“检测到”与“要接入”分开；CLI 仍默认全选。
pub fn plan_selected(targets: &Targets, selected: &[String]) -> Result<Value> {
    anyhow::ensure!(!selected.is_empty(), "请至少选择一个要接入的 agent");
    for agent in selected {
        anyhow::ensure!(SETUP_AGENTS.contains(&agent.as_str()), "不支持接入 agent: {agent}");
    }
    let exe = resolve_cli_exe()?.to_string_lossy().to_string();
    let mut agents = Vec::new();
    for agent in SETUP_AGENTS {
        if !selected.iter().any(|a| a == agent) {
            continue;
        }
        if !targets.agent_present(agent) {
            agents.push(json!({ "agent": agent, "status": "skip", "reason": "未检测到配置目录" }));
            continue;
        }
        // 这些 agent 只注册经过本机或官方文档确认的 MCP 配置；它们的全局指令
        // 发现与覆盖规则未逐一验证，不在 setup 中猜路径写入。
        if matches!(agent, "zcode" | "kimi" | "gemini" | "cursor" | "hermes") {
            let (st, detail) = mcp_status(targets.registered_mcp_command(agent));
            let path = match agent {
                "zcode" => &targets.zcode_config,
                "kimi" => &targets.kimi_mcp,
                "gemini" => &targets.gemini_settings,
                "cursor" => &targets.cursor_mcp,
                _ => &targets.hermes_config,
            };
            agents.push(json!({
                "agent": agent,
                "status": "detected",
                "actions": [
                    { "kind": "register_mcp", "path": path,
                      "status": st, "detail": detail },
                ],
                "mcp_entry": { "command": exe, "args": ["mcp"] },
            }));
            continue;
        }
        let (mcp_path, mcp_status_val) = match agent {
            "claude" => {
                let (st, detail) = mcp_status(targets.registered_mcp_command("claude"));
                (targets.claude_json.clone(), json!({ "status": st, "detail": detail }))
            }
            _ => {
                let (st, detail) = mcp_status(targets.registered_mcp_command("codex"));
                (targets.codex_config.clone(), json!({ "status": st, "detail": detail }))
            }
        };
        let (md_path, md_status) = match agent {
            "claude" => (
                targets.claude_md.clone(),
                instruction_status(&std::fs::read_to_string(&targets.claude_md).unwrap_or_default()),
            ),
            _ => (
                targets.codex_agents.clone(),
                instruction_status(&std::fs::read_to_string(&targets.codex_agents).unwrap_or_default()),
            ),
        };
        agents.push(json!({
            "agent": agent,
            "status": "detected",
            "actions": [
                { "kind": "register_mcp", "path": mcp_path,
                  "status": mcp_status_val["status"], "detail": mcp_status_val["detail"] },
                { "kind": "global_instructions", "path": md_path, "status": md_status,
                  "detail": if md_status == "stale" { json!("指令块是旧版本，重放会原位替换并保留你的其他内容") } else { Value::Null } },
            ],
            "mcp_entry": { "command": exe, "args": ["mcp"] },
        }));
    }
    Ok(json!({
        "agents": agents,
        "consequences": "将修改上述 agent 配置文件（已存在的内容一律先备份为 .bak-YYYYMMDD-HHMMSS）；重复执行幂等。",
    }))
}

pub fn plan(targets: &Targets) -> Result<Value> {
    plan_selected(targets, &SETUP_AGENTS.map(str::to_string))
}

/// 门控要素 3/4：备份后执行。`confirmed` 由调用方保证。
pub fn execute_selected(targets: &Targets, selected: &[String]) -> Result<Value> {
    let plan = plan_selected(targets, selected)?;
    let exe = resolve_cli_exe()?.to_string_lossy().to_string();
    let mut results = Vec::new();

    for agent in plan["agents"].as_array().cloned().unwrap_or_default() {
        if agent["status"] != "detected" {
            results.push(agent);
            continue;
        }
        let name = agent["agent"].as_str().unwrap_or_default().to_string();
        let mut done = Vec::new();
        for action in agent["actions"].as_array().cloned().unwrap_or_default() {
            if action["status"] == "done" {
                done.push(json!({ "kind": action["kind"], "status": "already_done" }));
                continue;
            }
            let path = PathBuf::from(action["path"].as_str().unwrap_or_default());
            let kind = action["kind"].as_str().unwrap_or_default();
            match kind {
                "register_mcp" if name == "claude" => register_claude_mcp(&path, &exe)?,
                "register_mcp" if name == "zcode" => register_zcode_mcp(&path, &exe)?,
                "register_mcp" if matches!(name.as_str(), "kimi" | "gemini" | "cursor") => register_mcp_servers_json(&path, &exe)?,
                "register_mcp" if name == "hermes" => register_hermes_mcp(&path, &exe)?,
                "register_mcp" => register_codex_mcp(&path, &exe)?,
                _ => write_instructions(&path)?,
            }
            done.push(json!({ "kind": kind, "status": "done", "path": path }));
        }
        results.push(json!({ "agent": name, "status": "configured", "actions": done }));
    }
    Ok(json!({ "agents": results, "verify": "新开一个 agent 会话，在 MCP 或工具列表确认 yourmem；Claude Code/Codex 还会按全局指令在历史问题中主动调用。" }))
}

pub fn execute(targets: &Targets) -> Result<Value> {
    execute_selected(targets, &SETUP_AGENTS.map(str::to_string))
}

/// 追加式后缀，完整保留原文件名（`foo.md` → `foo.md.bak-…`）。
/// 不能用 `with_extension`：它是替换最后一个扩展名，会把 `.md` 吃掉。
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// 写前备份 + 写 tmp 再 rename。返回 .bak 路径（若原件存在）。
fn backup_then_write(path: &Path, content: &[u8]) -> Result<Option<PathBuf>> {
    let bak = if path.is_file() {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let bak = with_suffix(path, &format!("bak-{ts}"));
        std::fs::copy(path, &bak).with_context(|| format!("备份 {}", bak.display()))?;
        Some(bak)
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        None
    };
    let tmp = with_suffix(path, &format!("yourmem-tmp.{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(bak)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_bundle_exe_detection() {
        // .app bundle 内路径
        assert!(is_gui_bundle_exe(Path::new(
            "/Applications/yourmem.app/Contents/MacOS/yourmem-app"
        )));
        assert!(is_gui_bundle_exe(Path::new(
            "/Users/x/Applications/yourmem.app/Contents/MacOS/yourmem"
        )));
        // 文件名是 yourmem-app（即使不在 bundle 路径里也判 GUI）
        assert!(is_gui_bundle_exe(Path::new("/usr/local/bin/yourmem-app")));
        // 普通 CLI 路径
        assert!(!is_gui_bundle_exe(Path::new("/usr/local/bin/yourmem")));
        assert!(!is_gui_bundle_exe(Path::new(
            "/Users/x/youmem/target/debug/yourmem"
        )));
    }

    #[test]
    fn instruction_marker_in_prose_is_not_a_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        // marker 出现在正文（且行首是多字节字符）：不是标题行——不得 panic、
        // 不得当旧块替换吞掉用户内容，块应走追加路径
        std::fs::write(
            &p,
            "※ 参考 yourmem 跨 agent 记忆库 的说明\n\n## 我的节\n保留\n",
        )
        .unwrap();
        write_instructions(&p).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("※ 参考 yourmem 跨 agent 记忆库 的说明"), "正文提及原样保留:\n{out}");
        assert!(out.contains("## 我的节\n保留"), "用户内容不吞:\n{out}");
        assert!(out.contains(INSTRUCTION_BLOCK.trim()), "块按无 marker 追加:\n{out}");
    }

    #[test]
    fn bak_and_tmp_names_keep_original_extension() {
        let p = Path::new("/tmp/x/foo.md");
        assert_eq!(
            with_suffix(p, "bak-20260823-235959"),
            PathBuf::from("/tmp/x/foo.md.bak-20260823-235959")
        );
        assert_eq!(
            with_suffix(p, "yourmem-tmp.123"),
            PathBuf::from("/tmp/x/foo.md.yourmem-tmp.123")
        );
        // 无扩展名文件名同样完整保留
        assert_eq!(
            with_suffix(Path::new("/tmp/x/session-abc"), "bak-1"),
            PathBuf::from("/tmp/x/session-abc.bak-1")
        );
    }

    fn hermes_targets(dir: &Path) -> Targets {
        let mut t = Targets::default();
        t.hermes_config = dir.join("config.yaml");
        // 其余目标指向不存在路径：plan 对 claude/codex 应报 skip
        // （claude 的 present 检查的是父目录，所以父目录也得不存在）
        t.claude_json = dir.join("nope-claude/.claude.json");
        t.claude_md = dir.join("nope-claude/CLAUDE.md");
        t.codex_config = dir.join("nope-codex/config.toml");
        t.codex_agents = dir.join("nope-codex/AGENTS.md");
        t.zcode_config = dir.join("nope-zcode/config.json");
        t.kimi_mcp = dir.join("nope-kimi/mcp.json");
        t.gemini_settings = dir.join("nope-gemini/settings.json");
        t.cursor_mcp = dir.join("nope-cursor/mcp.json");
        t
    }

    #[test]
    fn hermes_mcp_appends_when_section_missing_and_replaces_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        // 场景一：无 mcp_servers 段 → 整段追加
        std::fs::write(&p, "profile_name: default\nplugins:\n  enabled: []\n").unwrap();
        register_hermes_mcp(&p, "/opt/homebrew/bin/yourmem").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("mcp_servers:\n  yourmem:\n    command: /opt/homebrew/bin/yourmem"), "追加段缺失:\n{out}");
        assert!(out.starts_with("profile_name: default"), "用户原有内容不动:\n{out}");
        assert_eq!(hermes_registered_command(&out).as_deref(), Some("/opt/homebrew/bin/yourmem"));

        // 场景二：已有段与 stale 的 yourmem 块 → 原位替换，邻居完好
        std::fs::write(
            &p,
            "mcp_servers:\n  chrome-devtools:\n    command: npx\n    enabled: true\n  yourmem:\n    command: /old/yourmem-0.1.0\n    args:\n      - mcp\n    enabled: true\n  scansci-pdf:\n    command: /usr/bin/scansci\n    enabled: true\n",
        )
        .unwrap();
        register_hermes_mcp(&p, "/opt/homebrew/bin/yourmem").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(!out.contains("/old/yourmem-0.1.0"), "旧块应被替换:\n{out}");
        assert!(out.contains("chrome-devtools:") && out.contains("scansci-pdf:"), "邻居保留:\n{out}");
        assert!(out.contains("  scansci-pdf:"), "后邻块不得被吞:\n{out}");
        assert_eq!(hermes_registered_command(&out).as_deref(), Some("/opt/homebrew/bin/yourmem"));

        // 场景三：段存在但无 yourmem → 插入段键之后，不碰其他键
        std::fs::write(
            &p,
            "mcp_servers:\n  chrome-devtools:\n    command: npx\nother_top: 1\n",
        )
        .unwrap();
        register_hermes_mcp(&p, "/opt/homebrew/bin/yourmem").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("yourmem:") && out.contains("other_top: 1") && out.contains("chrome-devtools:"), "插入后结构完整:\n{out}");
    }

    #[test]
    fn hermes_mcp_preserves_user_disabled_flag() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "mcp_servers:\n  yourmem:\n    command: /old/yourmem\n    args:\n      - mcp\n    enabled: false\n",
        )
        .unwrap();
        register_hermes_mcp(&p, "/opt/homebrew/bin/yourmem").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("enabled: false"), "用户手关的开关必须保留:\n{out}");
        assert!(out.contains("command: /opt/homebrew/bin/yourmem"), "但命令版本要更新:\n{out}");
    }

    #[test]
    fn hermes_plan_and_execute_flow() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), "mcp_servers: {}\n").unwrap();
        let targets = hermes_targets(dir.path());
        let plan = plan(&targets).unwrap();
        let hermes = plan["agents"].as_array().unwrap().iter().find(|a| a["agent"] == "hermes").unwrap();
        assert_eq!(hermes["status"], "detected");
        assert_eq!(hermes["actions"][0]["status"], "todo");
        // claude/codex 未安装 → skip
        assert!(plan["agents"].as_array().unwrap().iter().any(|a| a["status"] == "skip"));

        let res = execute(&targets).unwrap();
        let h = res["agents"].as_array().unwrap().iter().find(|a| a["agent"] == "hermes").unwrap();
        assert_eq!(h["actions"][0]["status"], "done");
        let after1 = std::fs::read_to_string(&targets.hermes_config).unwrap();
        // 幂等：测试环境的 exe 探不出版本会判 stale 重注册，但产物内容必须逐字节稳定
        execute(&targets).unwrap();
        let after2 = std::fs::read_to_string(&targets.hermes_config).unwrap();
        assert_eq!(after1, after2, "重复 setup 不得改动文件内容");
    }

    #[test]
    fn json_mcp_registration_preserves_neighbors() {
        let dir = tempfile::tempdir().unwrap();
        let zcode = dir.path().join("zcode/config.json");
        let kimi = dir.path().join("kimi/mcp.json");
        let gemini = dir.path().join("gemini/settings.json");
        let cursor = dir.path().join("cursor/mcp.json");
        for path in [&zcode, &kimi, &gemini, &cursor] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(
            &zcode,
            r#"{"skills":{"enabled":true},"mcp":{"servers":{"other":{"type":"http","url":"https://example.invalid"},"yourmem":{"enabled":false,"command":"old"}}}}"#,
        ).unwrap();
        std::fs::write(&kimi, r#"{"mcpServers":{"kimi-cu":{"command":"node","args":["server.js"]}}}"#).unwrap();
        std::fs::write(&gemini, r#"{"hooks":{"BeforeTool":[]}}"#).unwrap();
        std::fs::write(&cursor, r#"{"mcpServers":{"other":{"command":"node"}}}"#).unwrap();

        register_zcode_mcp(&zcode, "C:/Program Files/yourmem/yourmem.exe").unwrap();
        for path in [&kimi, &gemini, &cursor] {
            register_mcp_servers_json(path, "C:/Program Files/yourmem/yourmem.exe").unwrap();
        }
        let z: Value = serde_json::from_str(&std::fs::read_to_string(&zcode).unwrap()).unwrap();
        let k: Value = serde_json::from_str(&std::fs::read_to_string(&kimi).unwrap()).unwrap();
        let g: Value = serde_json::from_str(&std::fs::read_to_string(&gemini).unwrap()).unwrap();
        let c: Value = serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
        assert_eq!(z["mcp"]["servers"]["yourmem"]["type"], "stdio");
        assert_eq!(z["mcp"]["servers"]["yourmem"]["enabled"], false);
        assert_eq!(z["mcp"]["servers"]["other"]["type"], "http");
        assert_eq!(z["skills"]["enabled"], true);
        assert_eq!(k["mcpServers"]["kimi-cu"]["command"], "node");
        assert_eq!(g["hooks"]["BeforeTool"], json!([]));
        assert_eq!(c["mcpServers"]["other"]["command"], "node");
        for v in [&k, &g, &c] {
            assert_eq!(v["mcpServers"]["yourmem"]["args"][0], "mcp");
        }
    }
}

fn read_json_config(path: &Path) -> Result<Value> {
    // 文件不存在 → 空对象起步；存在但读失败/不是合法 JSON → 拒绝：这份文件装着
    // 用户全部项目历史/配置，静默按空对象重写等于清空它（.bak 兜底救不了不知道的人）。
    let value = match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .with_context(|| format!("{} 不是合法 JSON，请先修复再 setup（不会自动重置）", path.display()))?,
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => {
            return Err(e).with_context(|| format!("读取 {} 失败（不会自动重置）", path.display()))
        }
    };
    Ok(value)
}

/// Claude Code user-scope MCP 注册表：~/.claude.json 的 mcpServers。
fn register_claude_mcp(path: &Path, exe: &str) -> Result<()> {
    let mut v = read_json_config(path)?;
    v["mcpServers"]["yourmem"] = json!({ "command": exe, "args": ["mcp"] });
    backup_then_write(path, serde_json::to_string_pretty(&v)?.as_bytes())?;
    Ok(())
}

/// ZCode 0.16.5：~/.zcode/cli/config.json 的 mcp.servers，stdio 需显式 type。
fn register_zcode_mcp(path: &Path, exe: &str) -> Result<()> {
    let mut v = read_json_config(path)?;
    let enabled = v.pointer("/mcp/servers/yourmem/enabled").and_then(Value::as_bool);
    let mut entry = json!({ "type": "stdio", "command": exe, "args": ["mcp"] });
    if enabled == Some(false) {
        entry["enabled"] = json!(false);
    }
    v["mcp"]["servers"]["yourmem"] = entry;
    backup_then_write(path, serde_json::to_string_pretty(&v)?.as_bytes())?;
    Ok(())
}

/// Kimi Code、Gemini CLI 与 Cursor 的用户级 JSON 配置均使用 mcpServers。
fn register_mcp_servers_json(path: &Path, exe: &str) -> Result<()> {
    let mut v = read_json_config(path)?;
    v["mcpServers"]["yourmem"] = json!({ "command": exe, "args": ["mcp"] });
    backup_then_write(path, serde_json::to_string_pretty(&v)?.as_bytes())?;
    Ok(())
}

/// Codex：~/.codex/config.toml 的 [mcp_servers.yourmem] 块。已存在（stale 重注册）时
/// 原位替换该块——只动主表自己的行，紧随其后的 [mcp_servers.yourmem.tools.*] 子表保留。
fn register_codex_mcp(path: &Path, exe: &str) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let block = format!("[mcp_servers.yourmem]\ncommand = {exe:?}\nargs = [\"mcp\"]");
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let new_content = if let Some(pos) =
        lines.iter().position(|l| is_yourmem_table_header(l.trim()))
    {
        let mut end = lines.len();
        for (i, l) in lines.iter().enumerate().skip(pos + 1) {
            if l.trim_start().starts_with('[') {
                end = i;
                break;
            }
        }
        lines.splice(pos..end, block.lines().map(str::to_string).collect::<Vec<_>>());
        let mut s = lines.join("\n");
        s.push('\n');
        s
    } else {
        // 追加路径自己控制接缝的空行；不做全局 \n\n\n 压缩——那会改动用户
        // 无关区域的原有排版（自检 B4）
        let mut s = content.trim_end_matches('\n').to_string();
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(&block);
        s.push('\n');
        s
    };
    backup_then_write(path, new_content.as_bytes())?;
    Ok(())
}

/// Hermes：~/.hermes/config.yaml 的 mcp_servers.yourmem 段（2 空格会话名、
/// 4 空格属性）。文本块操作而非 YAML 重写——她的 config.yaml 有注释与手排结构，
/// serde_yaml 往返会毁掉无关区域。已有 yourmem 块（stale 重注册）原位替换，
/// 用户手关的 `enabled: false` 保留（尊重她自己的开关）。
fn register_hermes_mcp(path: &Path, exe: &str) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    // 旧块里是否有关闭的 enabled：保留用户意图
    let enabled = hermes_registered_enabled(&content).unwrap_or(true);
    let block = format!(
        "  yourmem:\n    command: {exe}\n    args:\n      - mcp\n    enabled: {enabled}"
    );
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let ms = lines.iter().position(|l| {
        !l.trim_start().starts_with('#') && l.starts_with("mcp_servers:")
    });
    let new_content = match ms {
        None => {
            // 无 mcp_servers 段：整段追加
            let mut s = content.trim_end_matches('\n').to_string();
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str("mcp_servers:\n");
            s.push_str(&block);
            s.push('\n');
            s
        }
        Some(ms) => {
            match lines.iter().position(|l| {
                let indent = l.len() - l.trim_start().len();
                !l.trim().is_empty() && indent == 2 && l.trim() == "yourmem:"
            }) {
                None => {
                    // 段存在、yourmem 未注册：插在段键之后（她的子键排前面也没关系，
                    // YAML 映射无序）
                    let mut out: Vec<String> = Vec::new();
                    for (i, l) in lines.iter().enumerate() {
                        out.push(l.clone());
                        if i == ms {
                            out.extend(block.lines().map(str::to_string));
                        }
                    }
                    out.join("\n") + "\n"
                }
                Some(pos) => {
                    // yourmem 已在：原位替换到下一个 ≤2 空格缩进的非空行（或 EOF）
                    let mut end = lines.len();
                    for (i, l) in lines.iter().enumerate().skip(pos + 1) {
                        let indent = l.len() - l.trim_start().len();
                        if !l.trim().is_empty() && indent <= 2 {
                            end = i;
                            break;
                        }
                    }
                    lines.splice(pos..end, block.lines().map(str::to_string).collect::<Vec<_>>());
                    lines.join("\n") + "\n"
                }
            }
        }
    };
    backup_then_write(path, new_content.as_bytes())?;
    Ok(())
}

/// 读 yourmem 块里现有的 enabled 值（true/false）；没有或读不出 → None（调用方默认 true）。
fn hermes_registered_enabled(content: &str) -> Option<bool> {
    let lines: Vec<&str> = content.lines().collect();
    let ms = lines.iter().position(|l| {
        !l.trim_start().starts_with('#') && l.starts_with("mcp_servers:")
    })?;
    let mut in_block = false;
    for l in &lines[ms + 1..] {
        let indent = l.len() - l.trim_start().len();
        if l.trim().is_empty() {
            continue;
        }
        if indent <= 2 {
            if in_block {
                return None;
            }
            in_block = l.trim() == "yourmem:" && indent == 2;
            continue;
        }
        if in_block && indent >= 4 {
            if let Some((k, v)) = l.trim().split_once(':') {
                if k.trim() == "enabled" {
                    return Some(v.split('#').next().unwrap_or("").trim() == "true");
                }
            }
        }
    }
    None
}

/// 写全局指令块：无 marker 追加；有 marker 但非当前全文（stale）→ 旧块整体
/// 替换——从 marker 所在标题行首到下一个 `## ` 标题（或 EOF），用户其他内容不动。
fn write_instructions(path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    // 旧块定位只认标题行（行以 "## " 开头且含 marker）：正文提及不构成块，走
    // 追加路径；切片全落在行边界，多字节行首不会 byte-index panic。
    let block_line = content
        .match_indices(INSTRUCTION_MARKER)
        .map(|(i, _)| content[..i].rfind('\n').map(|p| p + 1).unwrap_or(0))
        .find(|&s| content[s..].starts_with("## "));
    let out = match block_line {
        Some(start) if !content.contains(INSTRUCTION_BLOCK.trim()) => {
            let end = content[start..]
                .find("\n## ")
                .map(|i| start + i)
                .unwrap_or(content.len());
            let mut s = String::with_capacity(content.len() + INSTRUCTION_BLOCK.len());
            s.push_str(&content[..start]);
            s.push_str(INSTRUCTION_BLOCK.trim_start());
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&content[end..]);
            s
        }
        _ => {
            let mut c = content;
            if !c.ends_with('\n') && !c.is_empty() {
                c.push('\n');
            }
            c.push_str(INSTRUCTION_BLOCK);
            c
        }
    };
    backup_then_write(path, out.as_bytes())?;
    Ok(())
}
