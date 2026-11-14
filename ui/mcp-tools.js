// Tools offered to agents over MCP: per-tool usage and a switch to stop
// offering one. Counting and storage live in the native commands.
const LABELS = {
  list_projects: "列出项目",
  get_project_context: "项目上下文",
  get_dossier: "项目卷宗",
  search_history: "搜索历史对话",
  read_session: "读取对话",
  read_native_memory: "读取原生记忆文件",
  create_handoff: "写交接",
  get_recent_work: "最近工作",
  save_memory: "保存记忆",
  search_memory: "搜索记忆",
  list_memories: "列出记忆",
  update_memory: "更改记忆状态",
  list_artifacts: "列出产物",
  list_tasks: "列出任务",
  create_task: "新建任务",
  get_task_context: "任务上下文",
  update_task: "更新任务",
  link_task: "关联任务与对话",
  unlink_task: "取消任务关联",
  close_work: "收尾并交接",
  add_task_verification: "记录任务验证",
  get_timeline: "事件时间线",
  get_changes: "增量变化",
  list_project_records: "列出项目记录",
  get_project_record: "读取项目记录",
  search_work: "搜索工作资产",
};

const escapeHtml = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

// 进行中的写入。设置页重画后，新页面的首次读取等它结束，读到的才是写入后的配置
let writing = Promise.resolve();

export function mcpToolsHtml() {
  return `
      <h2>提供给 agent 的工具</h2>
      <div class="memcard" id="mcp-tools">
        <div class="meta" style="margin-top:0">工具说明随每次请求发送给模型。停用长期不调用的工具可减少每个对话的固定开销；停用对新开的对话生效，可随时恢复。</div>
        <div id="mcp-tools-list"></div>
        <div class="meta" id="mcp-tools-summary"></div>
        <div id="mcp-tools-report" role="status" aria-live="polite"></div>
      </div>`;
}

export function bindMcpTools({ invoke, root, esc = escapeHtml }) {
  const q = (selector) => root.querySelector(selector);
  const list = q("#mcp-tools-list");
  // 设置页重画后容器换成新节点，这次绑定不再改动页面
  const current = () => q("#mcp-tools-list") === list;
  let data = { tools: [] };
  let busy = false;

  const size = (n) => {
    n = Number(n || 0);
    return n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : n >= 1024 ? `${(n / 1024).toFixed(1)} KB` : `${n} B`;
  };
  const day = (t) => {
    const d = new Date(t);
    if (Number.isNaN(d.getTime())) return "";
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  };

  // 表格只按 data 重画，不读回页面上的勾选状态；写入期间所有开关不可操作
  const draw = () => {
    if (!current()) return;
    const tools = data.tools || [];
    const days = Number(data.days) || 30;
    list.innerHTML = `<table>
      <tr><th>工具</th><th class="c-num">近 ${days} 天调用</th><th class="c-num">返回量</th><th class="c-num">说明字符数</th><th>状态</th></tr>
      ${tools.map((t) => `<tr style="${t.disabled ? "opacity:.5" : ""}">
        <td>${esc(t.name)} <span style="color:var(--dim)">${esc([LABELS[t.name], t.in_instructions ? "接入指令提到" : ""].filter(Boolean).join(" · "))}</span></td>
        <td class="c-num">${Number(t.calls) || 0}</td>
        <td class="c-num">${t.response_bytes ? size(t.response_bytes) : "—"}</td>
        <td class="c-num">${Number(t.definition_chars) || 0}</td>
        <td><label style="display:flex;align-items:center;gap:6px;cursor:pointer;white-space:nowrap">
          <input type="checkbox" data-mcp-tool="${esc(t.name)}" ${t.disabled ? "" : "checked"} ${busy ? "disabled" : ""} />
          <span style="font-size:12px;color:var(--dim)">${t.disabled ? "已停用" : "提供中"}</span>
        </label></td>
      </tr>`).join("")}</table>`;
    const total = tools.reduce((sum, t) => sum + (Number(t.definition_chars) || 0), 0);
    const off = tools.filter((t) => t.disabled).reduce((sum, t) => sum + (Number(t.definition_chars) || 0), 0);
    // 调用次数与返回量的记录起点不同：起点落在统计窗口内时分别注明
    const recent = (t) => {
      const d = t ? new Date(t) : null;
      return d && Date.now() - d.getTime() < days * 86400000 ? day(d) : "";
    };
    const callsFrom = recent(data.recorded_since);
    const bytesFrom = recent(data.bytes_recorded_since);
    q("#mcp-tools-summary").textContent =
      `工具说明合计 ${total} 字符${off ? `，其中已停用 ${off} 字符` : ""}。`
      + (!data.recorded_since ? "暂无调用记录。" : callsFrom ? `调用记录始于 ${callsFrom}。` : "")
      + (data.recorded_since && !data.bytes_recorded_since ? "返回量暂无记录。"
        : bytesFrom && data.bytes_recorded_since !== data.recorded_since ? `返回量自 ${bytesFrom} 起记录。` : "");
  };

  const report = (text) => {
    if (current()) q("#mcp-tools-report").textContent = text;
  };

  // 读取并重画；成功返回空串，失败返回原因。只采用最近一次读取的结果
  let latest = 0;
  const load = async () => {
    const mine = ++latest;
    await writing;
    try {
      const fresh = await invoke("mcp_tools");
      if (mine === latest) {
        data = fresh;
        draw();
      }
      return "";
    } catch (error) {
      return String(error);
    }
  };

  // 表格每次重画，勾选事件挂在容器上
  list.onchange = async (event) => {
    const name = event.target?.dataset?.mcpTool;
    const tool = (data.tools || []).find((t) => t.name === name);
    if (!tool || busy) return;
    const disabled = !tool.disabled;
    busy = true;
    tool.disabled = disabled;
    draw();
    const write = invoke("mcp_tool_set_disabled", { name, disabled });
    writing = write.then(() => {}, () => {});
    let message;
    try {
      await write;
      message = disabled
        ? `已停用 ${name}，对新开的对话生效。${tool.in_instructions ? "接入指令提到了这个工具，agent 按指令调用时会找不到它。" : ""}`
        : `已恢复 ${name}，对新开的对话生效。`;
    } catch (error) {
      tool.disabled = !disabled;
      message = `操作失败：${String(error)}`;
    }
    const failed = await load();
    busy = false;
    draw();
    report(failed ? `${message} 列表读取失败：${failed}` : message);
  };

  load().then((failed) => {
    if (failed) report(`读取失败：${failed}`);
  });
  return { load };
}
