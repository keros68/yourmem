<div align="center">

<img src="src-tauri/icons/128x128@2x.png" width="112" alt="yourmem 图标">

# yourmem

[English](README_en.md) · [下载](https://github.com/keros68/yourmem/releases/latest) · [快速开始](#快速开始) · [使用说明](docs/guide.md) · [开发说明](docs/development.md) · [许可证](#许可证)

**在本机归档和搜索多个 AI 编程 Agent 的对话，保存项目记忆，并通过 MCP 交给下一个 Agent。**

</div>

yourmem 是基于 Tauri 2 的桌面应用，支持 Windows 与 macOS。对话从各 Agent 的本地记录增量采集，原文与来源一并保存；不需要云端账号，不上传聊天和记忆。

<p align="center">
  <img src="docs/images/search.png" alt="yourmem 搜索页：跨 Agent 检索对话与工具调用">
</p>

## 功能

- **统一归档**：自动发现本机已有对话并增量采集，按项目、Agent 和消息类型浏览，跨来源全文搜索中英文。原工具删除历史文件后，已归档的内容仍可校验、导出和恢复。
- **对话谱系**：识别续聊、分支、压缩和子任务关系，用谱系图查看任务的延续过程；对话压缩后仍可取回压缩点之前的原文。
- **两类记忆**：备份 `MEMORY.md`、`AGENTS.md` 等原生记忆文件的历史版本；把决定、规则、经验和偏好整理成项目记忆，保留来源对话，支持确认、取代和归档。
- **项目进度**：项目卷宗汇总时间线、决定、产出文件、任务状态和进度文档；「今天」页按日期、项目和 Agent 查看活动并导出日报。可选的 BYOK API 整理先预览、确认后才写入记忆。
- **MCP 接入**：一键接入 Claude Code、Codex、ZCode、Kimi Code、Gemini CLI、Cursor 和 Hermes，Agent 可直接检索历史、读取项目上下文和写入交接；其他 stdio MCP 客户端可手动配置 `yourmem mcp`。
- **备份与迁移**：回收站、每周增量快照、完整备份打包与校验，可恢复到另一台电脑或与已有资料库合并；核心数据和备份可分别放在非系统盘。

## 快速开始

1. 打开 [Releases](https://github.com/keros68/yourmem/releases/latest)，按系统下载安装包：Windows x64 选 `yourmem_*_x64-setup.exe`，macOS Apple Silicon 选 `yourmem_*_aarch64.dmg`，Intel 选 `yourmem_*_x64.dmg`。
2. 安装并运行。安装包尚未签名，首次运行需按系统提示手动放行。
3. 首次启动分别选择核心数据和备份位置，并勾选要接入的 Agent（默认均不接入）。之后 yourmem 每 60 秒在后台增量采集一次。
4. 在「搜索」中查找历史，或让已接入的 Agent 调用 yourmem，例如：「用 yourmem 读取这个项目的上下文」「查找以前处理这个错误的对话」。

接入、备份、多机合并、命令行与数据位置见[使用说明](docs/guide.md)。

## 支持的 Agent

| Agent | 对话来源 | 续聊命令 | 写回原工具 |
| --- | --- | --- | --- |
| Claude Code | `~/.claude/projects` | ✅ | ✅ |
| Codex | `~/.codex/sessions` | ✅ | ✅ |
| OpenCode | `~/.local/share/opencode/opencode.db` | ✅ | ❌ |
| ZCode | `~/.zcode/cli/rollout` | ❌ | ❌ |
| Kimi Code | `~/.kimi-code/sessions` | 主对话 | ❌ |
| Hermes | `~/.hermes/state.db` | ✅ | ❌ |
| pi | `~/.pi/agent/sessions` | ✅ | ❌ |
| Antigravity CLI | `~/.gemini/antigravity-cli/brain` | ❌ | ❌ |

各来源可读取的内容有所区别，详见桌面端「设置 → 能力矩阵」。Trae 的本地数据已加密，暂不支持。

## 从源码构建

依赖 Rust（stable）；桌面端另需 Tauri 2 的系统依赖。

```bash
cargo build              # 命令行 yourmem
cargo test
cd src-tauri && cargo run  # 桌面端开发模式
```

目录结构与测试说明见[开发说明](docs/development.md)。

## 相关项目

- [Metrik](https://github.com/keros68/metrik)：查看各 Agent 的剩余额度、重置时间和 Token 用量。yourmem 不统计用量，需要看额度时使用 Metrik。

## 许可证

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)，Copyright © 2026 yourmem contributors。
