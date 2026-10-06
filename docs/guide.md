# yourmem 使用说明

## 安装与更新

从 [Releases](https://github.com/keros68/yourmem/releases/latest) 下载 Windows x64 安装包，或适用于 Apple Silicon 与 Intel 芯片的 macOS 安装包。安装包暂未签名：Windows 可能显示未知发布者提示；macOS 首次打开时可能需要在系统设置的“隐私与安全性”中确认。

1.1.9 起可在「设置 → 通用」中直接下载并安装更新。1.1.8 及更早版本需要先手动安装一次 1.1.9。

首次启动必须分别选择核心数据和备份位置，保存后才创建资料库。向导会列出检测到的 Agent，默认均不接入；只修改用户主动勾选的 Agent，之后也可在设置中补充。建议把两个目录放在空间充足的非系统盘。

## 接入 Agent

1. 打开「设置 → 接入」，点击「检测并预览」。
2. 查看计划后点击「确认执行接入」。yourmem 会修改所选 Agent 的 MCP 配置并写入全局指令，原配置自动备份。
3. 接入后可以直接对 Agent 说：

> 用 yourmem 读取这个项目的上下文。

> 查找以前处理这个错误的对话。

> 为当前项目保存一条交接。

一键接入支持 Claude Code、Codex、ZCode、Kimi Code、Gemini CLI、Cursor 和 Hermes。其他兼容 stdio MCP 的客户端可手动配置命令 `yourmem mcp`。

## 日常使用

- yourmem 启动后每 60 秒增量采集一次；点击侧栏「采集新对话」可立即采集。
- 关闭窗口后隐藏到系统托盘并继续采集。左键点击托盘图标重新打开窗口，右键菜单可选择打开或退出；退出后采集停止。
- 「今天」上方是当天概况，下方可按日期、项目和 Agent 查看活动、任务、产物和交接明细，并导出日报。
- 对话详情显示原生会话 ID 和续聊命令，可复制后在项目目录中执行。
- 删除的对话先进入回收站，可以恢复；彻底删除需要再次确认。
- 项目根目录下的 `CLAUDE.md`、`AGENTS.md` 与 `progress.md`、`handoff.md` 等进度文档会自动备份，卷宗中显示更新情况与过时提示。
- 可选的 BYOK API 整理只发送有界的项目摘要，不发送完整对话或文件内容；结果先预览，确认后才写入项目记忆。API Key 保存在系统凭据库。

## 数据保存在哪里

桌面版首次启动会保存一个很小的位置指针 `~/.yourmem-location`，核心数据写入向导选择的目录；`YOUMEM_HOME` 可在命令行或受管环境中覆盖该选择。备份目录单独选择。

对话原文集中保存在核心数据目录的对象库 `objects.db` 中并透明压缩。早期版本逐个文件保存的原文由桌面端在后台分批迁入，也可运行 `yourmem backup migrate` 一次完成。数据库只保留大型工具输出的首尾预览，完整内容仍可从归档还原。

yourmem 日常只读取各 Agent 的原始数据。只有一键接入和把已归档的对话写回原工具这两类操作会写入原工具的数据目录或配置；两者都先显示计划，由用户确认，并在修改前备份原文件。

yourmem 不需要云端账号，也不会把聊天和记忆上传到服务端。当前不提供多设备实时同步、数据加密或基于 embedding 的语义搜索。

## 备份与快照

「设置 → 存储与备份」中的日常增量快照保存在备份目录的 `snapshots-v1` 中，包含压缩数据库与原件。桌面端每周自动创建一份（`config.json` 的 `snapshot_interval_days` 可调整，设为 0 关闭）；不同日期共享相同原件，每次只复制新增部分。默认保留最近 3 份和最近 3 个月的月度快照，创建后自动清理更早数据；手动修改保留规则需先预览再确认。

迁移或恢复时，选择快照导出独立完整备份，再执行校验与恢复。恢复先校验数据库、版本和引用对象，再写入对象并提交数据库；对象复制失败时可排除磁盘或权限问题后重试，已完整复制的对象会被复用。

1.1.0 起完整备份采用 v2 格式，省略可重建的搜索索引，恢复时重建。恢复新包需要 1.1.0 或更新版本；原有 v1 备份仍可恢复。

## 在两台电脑之间合并资料库

这是一次手动合并，不会持续同步：

1. 两台电脑分别在「设置 → 存储与备份」中创建 `.tar.gz` 完整备份。主电脑在合并前先备份当前资料库。
2. 将两份备份放在主电脑可访问的位置。通常每台电脑选择最新的一份；较早的备份只用于找回历史，合并后可能使已删除的对话重新出现。
3. 在主电脑的「设置 → 存储与备份」中填写另一份 `.tar.gz` 路径，点击「校验」。校验通过后点击「合并恢复」，查看预览，再点击「确认合并」。

合并时，同 ID 对话以消息较多的一侧为准，消息数相等时保留目标库版本；同 ID 项目记忆保留目标库版本。两台电脑的项目路径不同，可能会被识别为两个项目。

## 搜索范围

搜索默认采用轻量索引，工具输出正文中的长关键词可能无法命中；「设置 → 存储与备份 → 高级」中可开启工具输出全文索引。每条消息最多检索前 20 万字符，完整原文可从对话详情导出。

## 命令行

桌面端覆盖日常使用，命令行适合脚本、诊断和批量操作。除显式选择 Markdown 的导出外，命令结果均为 JSON。

```bash
yourmem import                         # 采集一次
yourmem watch --interval 5             # 持续采集
yourmem search "关键词"                 # 跨来源全文搜索
yourmem context [项目名]                # 读取项目上下文
yourmem dossier [项目名] --markdown     # 导出项目卷宗
yourmem docs [--project 项目名]         # 查看项目文档的更新情况与过时提示
yourmem docs track docs/plan.md         # 跟踪其他进度文档
yourmem session trash                  # 查看回收站
yourmem memory add --type decision --content "结论"
yourmem backup db                      # 创建数据库快照
yourmem backup migrate                 # 把早期版本的原文文件迁入对象库
yourmem bundle create -o yourmem.tar.gz
yourmem snapshot create                # 创建日常增量快照
yourmem snapshot list                  # 查看快照及仓库占用
yourmem snapshot export <ID> -o move.tar.gz
yourmem snapshot plan --keep-recent 3 --keep-monthly 3
yourmem doctor                         # 本地自检（桌面端每天在后台运行）
yourmem teardown                       # 解除全部 Agent 接入
yourmem teardown --delete-data         # 再删除核心数据
yourmem teardown --delete-data --delete-backups
```

按项目导出（`bundle create --project 项目名`）仅包含所选项目的记忆与原生记忆修订，全局记忆不随包携带。全部命令和参数见 `yourmem --help`。

## 卸载

「设置 → 接入」中的「解除接入与卸载」会移除 Claude Code、Codex、ZCode、Kimi Code、Gemini CLI、Cursor 和 Hermes 中的 yourmem 配置，并可选择同时永久删除核心数据与备份。各 Agent 自己的原始对话不会删除。
