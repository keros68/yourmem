# Agent 全局指令片段

MCP 注册只解决"能用"，这段指令解决"知道什么时候该用"。实测（codex exec）：
没有它，agent 会只搜当前仓库就回答"没做过"；加上后会先查 yourmem 再回答。

## Codex：`~/.codex/AGENTS.md` 追加

```markdown
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
```

## Claude Code：`~/.claude/CLAUDE.md` 写入同样内容

## Hermes：`~/.hermes/config.yaml` 的 mcp_servers 段注册（0.3.9 起）

Hermes 是 MCP 消费端但**不写全局指令块**（自带人格/提示词体系，工具自动注册后
按工具描述决定何时用）。配置形态（2 空格会话名 / 4 空格属性；`enabled: false`
是用户侧开关，setup 重注册时保留）：

```yaml
mcp_servers:
  yourmem:
    command: /opt/homebrew/bin/yourmem
    args:
      - mcp
    enabled: true
```

注册后 hermes 与 Claude/Codex 共享同一个记忆库——hermes 自身的会话是数据源之一
（`hermes` agent），也能搜其他 agent 的历史、读项目卷宗、写记忆。子代理
（`inherit_mcp_toolsets: true`）自动继承。

经验卡内容约定：适用条件、失败尝试、Why、How to apply、实际验证结果、来源与复查条件。未验证的结论标为待验证。工具描述同步采用此约定。
