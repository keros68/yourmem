# yourmem 开发说明

核心程序使用 Rust，桌面端使用 Tauri 2，数据保存在 SQLite 和本地对象库中。

## 目录结构

| 路径 | 内容 |
| --- | --- |
| `src/` | 核心库与命令行 `yourmem`：采集、搜索、记忆、备份、MCP 服务 |
| `src/adapters/` | 各 Agent 的对话解析，每个 Agent 一个文件 |
| `src-tauri/` | 桌面端 Tauri 外壳与命令 |
| `ui/` | 桌面端静态前端（原生 ES module，无构建步骤） |
| `tests/` | Rust 集成测试与前端 `*.test.cjs` 测试 |

## 构建与测试

```bash
cargo build
cargo test
node --test tests/*.test.cjs   # 前端测试
```

桌面端：

```bash
cd src-tauri
cargo run                          # 开发模式
npx tauri build --bundles nsis     # Windows 安装包
```

前端文件在构建时打进二进制，修改 `ui/` 后需要重新构建 `src-tauri` 才能在桌面端看到效果。

## 手动验证

使用沙盒数据目录，避免写入日常资料库：

```bash
export YOUMEM_HOME=$PWD/.sandbox
./target/debug/yourmem import
./target/debug/yourmem search "关键词"
./target/debug/yourmem memory list
./target/debug/yourmem doctor
```

各 Agent 的数据目录可用 `YOUMEM_CLAUDE_DIR`、`YOUMEM_CODEX_DIR`、`YOUMEM_ZCODE_DIR`、`YOUMEM_KIMI_DIR`、`YOUMEM_PI_DIR`、`YOUMEM_ANTIGRAVITY_DIR`、`YOUMEM_OPENCODE_DB`、`YOUMEM_HERMES_DB` 覆盖。

## 平台说明

- [Windows 支持与实测记录](WINDOWS.md)
