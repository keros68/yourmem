const esc = (s) => String(s ?? "").replace(/[&<>\"]/g, (c) => ({
  "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;",
}[c]));

const fmtTime = (t) => (t ? String(t).replace("T", " ").slice(11, 16) : "—");
const clipped = (s, max = 180) => {
  const text = String(s ?? "").replace(/\s+/g, " ").trim();
  return text.length > max ? `${text.slice(0, max)}…` : text;
};
const list = (items, empty) => items?.length
  ? `<ul>${items.map((x) => `<li>${esc(x)}</li>`).join("")}</ul>`
  : `<div class="work-empty">${empty}</div>`;

const activityStamp = (a) => String(a?.ended_at || a?.started_at || "");

const agentDot = (agent) => `<span class="activity-agent ${esc(agent)}"><i></i>${esc(agent)}</span>`;

export function todayOverviewHtml(digest) {
  const projects = digest?.project_activity || [];
  const recent = projects.flatMap((p) => (p.activities || []).map((a) => ({ ...a, project: p.project })))
    .sort((a, b) => activityStamp(b).localeCompare(activityStamp(a))).slice(0, 5);
  const tasks = (digest?.open_tasks || []).slice(0, 3);
  const handoffs = (digest?.recent_handoffs || []).slice(0, 2);
  const agentsToday = new Set(projects.flatMap((p) => (p.activities || []).map((a) => a.agent))).size;
  const stats = [
    ["folder", projects.length, "活跃项目", "今日有对话"],
    ["message-circle", digest?.sessions || 0, "新增对话", `${agentsToday} 个 Agent`],
    ["archive", digest?.messages || 0, "消息", "今日活跃会话"],
    ["file-text", digest?.artifacts_added || 0, "产物", "今日新增"],
  ];
  const statHtml = stats.map(([icon, value, label, hint]) => `<article class="today-stat">
    <span class="today-stat-icon"><img src="icons/${icon}.svg" alt=""></span>
    <strong>${Number(value).toLocaleString("zh-CN")}</strong><span>${label}</span><small>${hint}</small>
  </article>`).join("");
  const recentHtml = recent.length ? recent.map((a) => `<button class="today-activity" data-work-session="${esc(a.session_id)}">
    <time>${fmtTime(activityStamp(a))}</time><span class="today-activity-main"><strong>${esc(clipped(a.title, 90) || "未命名对话")}</strong>
    <span>${esc(clipped(a.tail, 150) || "尚无 Agent 回复")}</span></span>${agentDot(a.agent)}</button>`).join("")
    : '<div class="today-empty">今天还没有活动</div>';
  const taskHtml = tasks.length ? tasks.map((t) => `<div class="today-task"><img src="icons/square.svg" alt=""><span><b>${esc(clipped(t.content, 76))}</b><small>${esc(t.project || "全局")}${t.agent ? ` · ${esc(t.agent)}` : ""}</small></span></div>`).join("")
    : '<div class="today-empty">暂无未完成任务</div>';
  const handoffHtml = handoffs.length ? handoffs.map((h) => `<article class="today-handoff"><header><b>${esc(h.project)}</b><time>${fmtTime(h.created_at)}</time></header><p>${esc(clipped(h.next_steps || h.title, 120))}</p></article>`).join("")
    : '<div class="today-empty">暂无交接</div>';
  return `<div class="today-heading"><div><span>${esc(digest?.day || "")}</span><h1>今天 <em>Today</em></h1><p>快速掌握今天发生了什么；需要逐项追踪时进入动态。</p></div>
    <button class="btn primary" id="open-activity">查看动态</button></div>
    <div class="today-stats">${statHtml}</div>
    <div class="today-overview"><section class="today-panel today-recent"><header><div><h2>最近活动</h2><p>按时间汇总各 Agent 的最新工作</p></div><button id="open-activity-all">查看全部 →</button></header><div>${recentHtml}</div></section>
      <div class="today-side"><section class="today-panel"><header><div><h2>未完成任务</h2><p>${digest?.open_tasks?.length || 0} 项需要继续推进</p></div></header><div class="today-task-list">${taskHtml}</div></section>
      <section class="today-panel"><header><div><h2>最近交接</h2><p>跨 Agent 延续工作的入口</p></div></header><div>${handoffHtml}</div></section></div></div>`;
}

const activityTabs = (project, active) => [
  ["activities", "活动", project?.activities?.length || 0],
  ["tasks", "任务", project?.open_tasks?.length || 0],
  ["artifacts", "产物", project?.artifacts?.length || 0],
  ["handoff", "交接", project?.latest_handoff ? 1 : 0],
].map(([id, label, count]) => `<button class="${active === id ? "on" : ""}" data-activity-tab="${id}">${label}<span>${count}</span></button>`).join("");

function activityDetailHtml(project, agent, tab) {
  if (!project) return '<div class="activity-no-project">当天没有符合筛选条件的项目</div>';
  const agents = (project.agents || []).map((a) => agentDot(a.agent)).join("");
  let body = "";
  if (tab === "activities") {
    const rows = (project.activities || []).filter((a) => agent === "all" || a.agent === agent);
    body = rows.length ? rows.map((a) => `<button class="activity-row" data-work-session="${esc(a.session_id)}"><time>${fmtTime(activityStamp(a))}</time>
      <span><strong>${esc(clipped(a.title, 100) || "未命名对话")}</strong><small>${esc(clipped(a.tail, 190) || "尚无 Agent 回复")}</small></span>${agentDot(a.agent)}</button>`).join("")
      : '<div class="activity-no-project">当前 Agent 没有活动</div>';
  } else if (tab === "tasks") {
    body = (project.open_tasks || []).length ? project.open_tasks.map((t) => `<article class="activity-card"><span class="activity-state">进行中</span><div><strong>${esc(clipped(t.content, 150))}</strong><small>${esc(t.project || project.project)}</small></div></article>`).join("")
      : '<div class="activity-no-project">这个项目没有未完成任务</div>';
  } else if (tab === "artifacts") {
    const rows = (project.artifacts || []).filter((a) => agent === "all" || a.agent === agent);
    body = rows.length ? rows.map((a) => `<button class="activity-card clickable" data-work-session="${esc(a.session_id)}"><img src="icons/file-text.svg" alt=""><div><strong title="${esc(a.path)}">${esc(clipped(a.path, 120))}</strong><small>${esc(a.tool || "产物")} · ${fmtTime(a.created_at)} · ${esc(a.agent)}</small></div></button>`).join("")
      : '<div class="activity-no-project">当前筛选下没有产物</div>';
  } else {
    const h = project.latest_handoff;
    body = h ? `<article class="activity-handoff"><header><strong>${esc(h.title)}</strong><time>${fmtTime(h.created_at)}</time></header><p>${esc(h.next_steps || "未记录下一步")}</p></article>`
      : '<div class="activity-no-project">这个项目还没有交接记录</div>';
  }
  return `<div class="activity-detail-head"><div><div class="activity-title"><h2>${esc(project.project)}</h2>${agents}</div><p class="activity-path" title="${esc(project.path)}">${esc(project.path)}</p><p>${esc(clipped(project.activities?.[0]?.tail || "当天有活动记录", 120))}</p></div>
    <div class="activity-counts"><span><b>${project.sessions}</b>对话</span><span><b>${project.messages}</b>消息</span></div></div>
    <nav class="activity-tabs">${activityTabs(project, tab)}</nav><div class="activity-tab-body">${body}</div>`;
}

export function activityPageHtml(digest, state = {}) {
  const agent = state.agent || "all";
  const filterProject = state.project || "all";
  const allProjects = digest?.project_activity || [];
  const agentProjects = agent === "all" ? allProjects : allProjects.filter((p) => (p.agents || []).some((a) => a.agent === agent));
  const projects = filterProject === "all" ? agentProjects : agentProjects.filter((p) => String(p.project_id) === String(filterProject));
  const selected = projects.find((p) => String(p.project_id) === String(state.selected)) || projects[0] || null;
  const projectOptions = allProjects.map((p) => `<option value="${p.project_id}" ${String(filterProject) === String(p.project_id) ? "selected" : ""}>${esc(p.project)}</option>`).join("");
  const knownAgents = [...new Set(allProjects.flatMap((p) => (p.agents || []).map((a) => a.agent)))].sort();
  const agentOptions = knownAgents.map((a) => `<option value="${esc(a)}" ${agent === a ? "selected" : ""}>${esc(a)}</option>`).join("");
  const projectRows = projects.map((p) => `<button class="activity-project ${selected === p ? "on" : ""}" data-activity-project="${p.project_id}"><header><strong>${esc(p.project)}</strong><time>${fmtTime(activityStamp(p.activities?.[0]))}</time></header>
    <p>${esc(clipped(p.activities?.[0]?.tail || "当天有活动记录", 92))}</p><footer><span>${p.sessions} 对话</span><span>${p.messages} 消息</span><i>${(p.agents || []).map((a) => `<b class="dot ${esc(a.agent)}"></b>`).join("")}</i></footer></button>`).join("");
  const today = state.today || "";
  return `<div class="activity-heading"><span>Daily activity</span><h1>动态</h1><p>按日期、项目和 Agent 查看工作进展；每条信息都可以回到来源对话。</p></div>
    <div class="activity-toolbar"><div class="activity-date"><button id="activity-prev" title="前一天"><img src="icons/chevron-left.svg" alt=""></button><label><input id="activity-day" type="date" value="${esc(digest?.day || "")}" ${today ? `max="${esc(today)}"` : ""}></label><button id="activity-next" title="后一天" ${today && digest?.day >= today ? "disabled" : ""}><img src="icons/chevron-right.svg" alt=""></button></div>
      <label class="activity-select"><span>项目</span><select id="activity-project-filter"><option value="all">全部项目</option>${projectOptions}</select></label>
      <label class="activity-select"><span>Agent</span><select id="activity-agent-filter"><option value="all">全部 Agent</option>${agentOptions}</select></label>
      <span class="activity-toolbar-spacer"></span><button class="btn activity-export" id="activity-export"><img src="icons/file-export.svg" alt="">导出日报</button><button class="btn primary" id="activity-ai" ${!allProjects.length ? "disabled" : ""}>AI 整理</button></div>
    <div class="activity-workspace"><aside><header><div><h2>活跃项目</h2><span>${projects.length}</span></div><p>按最近活动排序</p></header><div class="activity-project-list">${projectRows || '<div class="activity-no-project">当天没有项目活动</div>'}</div></aside>
      <section class="activity-detail">${activityDetailHtml(selected, agent, state.tab || "activities")}</section></div>`;
}

const section = (label, values) => values?.length
  ? `<div class="ai-section"><b>${label}</b>${list(values, "")}</div>` : "";

export function aiSummaryHtml(response, saved = new Set()) {
  const result = response?.result;
  if (!result) return "";
  const labels = { completed: "已完成", in_progress: "进行中", blocked: "受阻", mixed: "有进展也有遗留" };
  return `<div class="ai-result-head"><div><b>AI 整理建议</b>${result.overview ? `<p>${esc(result.overview)}</p>` : ""}</div>
    <span>使用 ${response.source_projects || 0} 个项目 / ${response.source_sessions || 0} 个来源对话 · 输入 ${response.input_chars || 0} 字符${response.truncated ? " · 已按上限截断" : ""}</span></div>
    <div class="ai-result-grid">${(result.projects || []).map((p, i) => `<article class="ai-project">
      <header><h3>${esc(p.project)}</h3><span class="pill">${labels[p.status] || "进行中"}</span></header>
      <p class="ai-summary">${esc(p.summary || "")}</p>
      ${section("已完成", p.completed)}${section("进行中", p.in_progress)}${section("受阻", p.blocked)}
      ${section("决定", p.decisions)}${section("下一步", p.next_steps)}
      <footer><span>来源：${(p.sources || []).map((sid, n) => `<button class="work-link" data-ai-source="${esc(sid)}">对话 ${n + 1}</button>`).join(" · ")}</span>
        <button class="btn small ${saved.has(i) ? "" : "primary"}" data-ai-save="${i}" ${saved.has(i) ? "disabled" : ""}>${saved.has(i) ? "已保存" : "保存为项目摘要"}</button></footer>
    </article>`).join("")}</div>`;
}
