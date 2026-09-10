# Windows 支持（0.4.5 代码适配 → 0.4.6 真机验证收口，2026-09-03）

mac 侧的代码适配于 0.4.5 落地；本文档原为真机接续清单，**2026-09-03 已在
Windows 11（x64, MSVC rustc 1.97）真机上全部验证并修复所发现问题**，现作为
Windows 支持的现状说明与实测记录保留。原则不变：agent 数据目录布局没有
真实样本前不改解析代码。

## 1. 首次编译与测试（✅ 已验证）

- 环境：rustup `x86_64-pc-windows-msvc` + VS Build Tools 2022（C++ 工作负载，
  rusqlite bundled 编 SQLite C 源码）；`cargo build` 首次约 40s，Tauri app
  首次约 2 分钟。
- **clone 前 `git config --global core.autocrlf false`**（Git for Windows
  系统级默认 true，实测会把工作区搞坏；本机已改全局）。
- 首跑测试 111 个中红 3 个 + 1 个 warning，全数修复：
  1. `dossier::tests::day_bounds_havana_midnight_fallback_does_not_error`——
     依赖 Unix 的 `TZ` 环境变量语义，Windows 上 chrono::Local 不解析 IANA
     风格 TZ（回退系统时区）。已 `#[cfg(unix)]` 门控；跨平台换算逻辑由
     `day_bounds_respect_local_offset`（纯 FixedOffset）覆盖。
  2. `tests/gc.rs` 4 例全红（os error 267）——`purge_session` 备份目录名
     直接拼 `session_id`（`agent:id` 含冒号），Windows 文件名禁 `:`。已加
     `slug_session_id` 三端统一净化（先例：桌面 session_export 的 `:`→`_`）。
  3. `tests/integration.rs::native_memory_files_backup`——fixture 把
     `proj.display()`（反斜杠路径）直接嵌 JSON 字符串，`\U` 成非法转义整行
     被拒。fixture 改用正斜杠（Windows API 通吃）；同时 `memfiles.rs
     claude_project_encodings` 补 Windows 反斜杠/盘符冒号变体（防御式 +
     磁盘存在性兜底，mac 零影响）。
  4. `tests/bundle.rs` 的 `use serde_json::json` 在 Windows 下 unused
     （两个使用者都被 unix 门控）——import 同步门控。
- 修复后 10 个测试 target 全绿、零 warning。

## 2. 真机验证清单（✅ 全部完成，含实测结论）

1. **home 解析 ✅**：实测 Git Bash 环境 `HOME=/c/Users/<user>`（POSIX 风格），
   原 HOME 优先的回退链会把 data_home 解析到无效路径（Windows API 错解为
   当前盘符根下 `\c\...`）。**已改为 Windows 上 USERPROFILE 最先**（HOME 在
   Windows 只于 MSYS/Cygwin 类环境出现且可能是 POSIX 风格）。实测三种组合
   （POSIX HOME / 无 HOME / 无 HOME+USERPROFILE）全部正确落
   `%USERPROFILE%\.yourmem`（第三组走 HOMEDRIVE+HOMEPATH 兜底）。
2. **数据源检测 ✅**：`agents list` 实测 claude/codex/zcode/kimi 四个文件型
   全部 `detected: true`（`%USERPROFILE%` 下 dotfile 布局假设成立：
   `.claude\projects`、`.codex\sessions`、`.zcode\cli\rollout`、
   `.kimi-code\sessions` 实际存在）；watchlist 亦正常（本机实测命中
   观察名单里的已装 agent）。
3. **opencode ❌ 维持门禁**：本机 `.local/share/opencode` 下只有空 log/
   repos，无 opencode.db——**仍未拿到真实样本，不改解析代码**。附带实测：
   显式指定（YOUMEM_OPENCODE_DB / 旗标）指向不存在的库会 fail-loud 报错，
   默认路径缺失则静默跳过（未装=正常态），行为正确。
4. **活跃文件采集 ✅（实测推翻预判）**：PowerShell 以
   `FileShare.None` 独占写打开会话文件时，Rust 读取报的**不是 error 5
   PermissionDenied，而是 os error 32/33（ERROR_SHARING_VIOLATION /
   ERROR_LOCK_VIOLATION）**。已在 `ingest.rs` 实现 `is_file_busy` 容错
   （error 5 + Windows 分支比对 raw_os_error 32/33）：占用文件跳过本轮
   （`files_skipped` 计数，stderr 提示），offset 未推进下轮增量补采——
   实测锁释放后一次 import 即补齐（messages_added=1），不再毒化整轮。
5. **桌面 app ✅**：
   - 谱系图放大覆盖层：**WebView2（Chromium)下 CSS zoom 影响布局，缩放后
     滚动范围正确，与 WKWebView 行为一致**（＋/− 步进实测正常）；
   - Esc 捕获链正常：覆盖层关闭、drawer 保留、无穿透；
   - `explorer /select,`（逗号无空格、单 arg）reveal 正常，路径含正斜杠
     亦可。
   - **真机修复三处（用户反馈）**：①同步 Tauri 命令跑在主线程，大库导入
     时整窗冻结——`import_now`/`session_verify`/`session_export`/
     `session_writeback`/`bundle_*`/`setup_run`/`doctor`/`trash_purge`/
     `trash_empty_overdue` 全部 async 化（spawn_blocking）；②任务栏/标题栏
     图标 debug 构建缺失（图标资源只在 `tauri build` 时嵌入 exe）——
     setup 里 `set_icon(32x32.png)` 代码内设置（Cargo.toml 加 tauri
     `image-png` feature）；③标题栏白条——代码内 `set_theme(Dark)`
     （config 的 windows.theme 字段 schema 不认，已回退该写法）。
6. **setup 一键接入 ✅**：真实环境执行（claude.json / codex config.toml /
   CLAUDE.md / AGENTS.md 四文件），MCP 条目与指令块写入正确、用户原内容
   保留、时间戳 .bak 齐全、重复执行全部 already_done（幂等）。注册命令
   解析为 CLI exe 本体。**Windows 修复**：`resolve_cli_exe`/`is_gui_bundle_exe`
   此前只找裸名 `yourmem`（Windows 产物是 `yourmem.exe`，GUI 检测也按完整
   文件名比对 `yourmem-app` 匹配不上 `yourmem-app.exe`）——找不到 CLI 会
   退回把 app 本体注册进 agent 配置（8-30 事故的 Windows 变体）。已改
   file_stem 比对 + `yourmem`/`yourmem.exe` 双候选。
7. **打包 ✅**：`npx @tauri-apps/cli build --bundles nsis`（tauri-cli 2.11）
   产出 `src-tauri/target/release/bundle/nsis/yourmem_0.4.5_x64-setup.exe`。
   config 的 `bundle.targets` 保留 mac 的 app/dmg 没关系，命令行指定 nsis
   即可。NSIS 工具链由 tauri-cli 首次自动下载。

## 3. 过程中发现并修复的额外问题（跨平台）

- **CLI import 不合并 `config.json` extra_roots**：`run_import` 只拼
  旗标/env/默认四个根，`agents add` 登记的自定义根只有桌面 app/MCP 能采到
  （违反"import_defaults 是唯一入口"的约定）；CLI 侧也没有停用过滤。已加
  `ingest::apply_extra_roots_and_gates` 供 CLI 复用（extra_roots 合并 +
  disabled_agents 过滤）。
- 后台 CLI 大导入持 `.import.lock.db` 期间，app 的采集按钮等锁 30s——
  async 化后 UI 不冻结、超时报错可见；长导入期间这个等待是设计语义
  （fail-closed），不属于缺陷。

## 4. 已知边界（更新）

- hermes 是 macOS/Linux 生态（本机实测 `.hermes` 下无 state.db，检测正确
  报 not_found）；Windows 上 `agents disable hermes` 即可。
- opencode Windows 数据目录仍无真实样本（本机实测无 .db）；找到真实库后
  按 `adapters/opencode.rs default_db_path` 是否需要 Windows 分支处理，
  在此之前用 `YOUMEM_OPENCODE_DB`。
- **单库源只读打开的热 WAL 边界（2026-09-10 登记，未实测）**：opencode/hermes
  两个 adapter 对已存在的库走 `SQLITE_OPEN_READ_ONLY` 原地打开。agent 正常
  并发写时只读连接安全（SQLite 自身管理并发）；但若源进程崩溃遗留需恢复的
  热 WAL，只读连接无法执行恢复、open 直接报错——而两个 adapter 对"库存在
  但打开失败"是 fail-loud 传播，会毒化整轮 import（区别于文件型源已有
  `is_file_busy` 跳过容错，单库源不走那条路径）。外部佐证：resume-skills
  项目对同类问题（读取活跃 SQLite，场景是复制库文件）选择对不支持文件克隆
  的主机 fail-closed；we 是原地打开，常态更安全，崩溃遗留 WAL 场景同样暴露。
  真机遇到时修复方向：给单库源 open 加同 `is_file_busy` 语义的容错
  （打开失败跳过本轮、下轮补采），而不是照搬文件克隆。
- resume 命令只是字符串展示，无平台逻辑。
- bundle tar.gz 跨平台互换无已知问题。
- 签名/公证：不签名发布，NSIS 包有 SmartScreen 提示。
- debug 构建（`cargo run`）的窗口图标已由代码内 set_icon 补齐；release
  安装包的图标由资源嵌入承载，双保险。
