import { loadProjectRecall } from "./project-recall.js";
import { graphDepths } from "./graph-layout.js";
import { createSessionDrawer } from "./session-drawer.js";
import { snapshotPanelHtml, bindSnapshotPanel } from "./snapshot-panel.js";
import { todayOverviewHtml, activityPageHtml, aiSummaryHtml } from "./workbench.js";

const { invoke } = window.__TAURI__.core;

const $ = (sel) => document.querySelector(sel);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const fmtTime = (t) => (t ? String(t).replace("T", " ").slice(0, 16) : "—");
// 表格时间列双格式：窄窗口（媒体查询 ≤1239px）切 "MM-DD HH:MM" 省 ~30px/列，
// 完整时间在悬停 title 里，信息不丢
const fmtTimeCell = (t) => {
  if (!t) return "—";
  const full = String(t).replace("T", " ").slice(0, 16);
  return `<span title="${full}"><span class="t-full">${full}</span><span class="t-short">${full.slice(5)}</span></span>`;
};
const toast = (msg) => {
  const el = document.createElement("div");
  el.className = "toast";
  el.textContent = msg;
  document.body.appendChild(el);
  setTimeout(() => el.remove(), 2500);
};

function bindFolderPicker(buttonId, inputId, commitId) {
  const button = $(buttonId), input = $(inputId), commit = $(commitId);
  button.onclick = async () => {
    if (button.disabled || commit.disabled) return;
    button.disabled = commit.disabled = true;
    try {
      const path = await invoke("backup_dir_pick", { path: input.value.trim() });
      if (path !== null && input === $(inputId)) input.value = path;
    } catch (e) { toast(String(e)); }
    finally { button.disabled = commit.disabled = false; }
  };
}

// scrollbox 内表格的行对齐吸附（用户反馈 2026-08-31：固定 max-height 与行高
// 不整除，底部裁出半行）——渲染后按实际行高把高度收到「表头 + 整数行」；
// 内容不满上限时不干预，保持 CSS 里的 max-height。
function snapScrollboxTables(scope) {
  (scope || document).querySelectorAll(".scrollbox").forEach((box) => {
    const table = box.querySelector("table");
    if (!table || !table.rows.length) return;
    const max = parseFloat(getComputedStyle(box).maxHeight) || 0;
    if (!max || table.getBoundingClientRect().height <= max) return;
    const top = box.getBoundingClientRect().top;
    let fit = 0;
    for (const tr of table.rows) {
      const bottom = tr.getBoundingClientRect().bottom - top;
      if (bottom > max) break;
      fit = bottom;
    }
    if (fit > 0) box.style.maxHeight = `${fit}px`;
  });
}

// ---------------------------------------------------------------- drawer
const drawer = createSessionDrawer({ invoke, $, esc, fmtTime, toast, snapScrollboxTables,
  bindCopyButtons, lineageGraphHtml, afterGraphRender, armButton });
const openDrawer = drawer.open;
const closeDrawer = drawer.close;
const showSession = drawer.showSession;
const showMemoryFile = drawer.showMemoryFile;
$("#drawer-close").onclick = closeDrawer;
$("#drawer-backdrop").onclick = closeDrawer;
// CSP（default-src 'self'）禁内联 onclick 属性：来源跳转统一 data 属性 + 委托
document.addEventListener("click", (e) => {
  const t = e.target.closest("span.src[data-src]");
  if (t) showSession(t.dataset.src, t.dataset.line != null ? +t.dataset.line : null);
});

function bindCopyButtons(scope) {
  document.querySelectorAll(`${scope} [data-copy]`).forEach((b) => {
    b.onclick = async () => {
      await navigator.clipboard.writeText(b.dataset.copy);
      toast("已复制");
    };
  });
}

// ---------------------------------------------------------------- today
async function renderToday() {
  const dg = await invoke("daily_digest");
  $("#page-today").innerHTML = todayOverviewHtml(dg);
  document.querySelectorAll("#page-today [data-work-session]").forEach((b) => {
    b.onclick = () => showSession(b.dataset.workSession);
  });
  const open = () => document.querySelector('.nav[data-page="activity"]').click();
  $("#open-activity").onclick = open;
  $("#open-activity-all").onclick = open;
}

const localDay = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
let activityDay = localDay(new Date());
let activityDigest = null;
let activityState = { project: "all", agent: "all", selected: null, tab: "activities" };
let activityAiResponse = null;
const activitySaved = new Set();

const shiftDay = (day, offset) => {
  const d = new Date(`${day}T12:00:00`);
  d.setDate(d.getDate() + offset);
  return localDay(d);
};

function closeActivityAi() {
  document.getElementById("activity-ai-overlay")?.remove();
}

function showAiSetupReminder() {
  closeActivityAi();
  const ov = document.createElement("div");
  ov.id = "activity-ai-overlay";
  ov.innerHTML = `<section class="ai-setup-reminder"><header><div><h2>需要先配置 AI</h2><p>AI 整理是可选功能，不影响本地动态与日报。</p></div><button id="activity-ai-close" title="关闭"><img src="icons/x.svg" alt=""></button></header>
    <div class="ai-setup-reminder-body"><p>请填写兼容 OpenAI 的 API 地址、模型名称和 API Key，保存后即可整理当天活动。</p>
      <div class="activity-ai-actions"><button class="btn" id="activity-ai-later">稍后</button><button class="btn primary" id="activity-ai-settings">前往 AI 设置</button></div></div></section>`;
  document.body.appendChild(ov);
  const close = () => ov.remove();
  $("#activity-ai-close").onclick = close;
  $("#activity-ai-later").onclick = close;
  $("#activity-ai-settings").onclick = () => {
    close();
    settingsTab = "ai";
    document.querySelector('.nav[data-page="settings"]').click();
  };
  ov.onclick = (e) => { if (e.target === ov) close(); };
}

function bindActivityAi(digest) {
  const out = $("#activity-ai-result");
  out.innerHTML = aiSummaryHtml(activityAiResponse, activitySaved);
  out.querySelectorAll("[data-ai-source]").forEach((b) => { b.onclick = () => showSession(b.dataset.aiSource); });
  out.querySelectorAll("[data-ai-save]").forEach((b) => {
    b.onclick = async () => {
      const i = Number(b.dataset.aiSave);
      b.disabled = true;
      try {
        await invoke("ai_summary_save", { day: activityAiResponse.day || digest.day, summary: activityAiResponse.result.projects[i] });
        activitySaved.add(i); bindActivityAi(digest); toast("已保存为项目记忆");
      } catch (e) { b.disabled = false; toast(String(e)); }
    };
  });
}

async function showActivityAi(digest) {
  let config = null;
  try { config = await invoke("ai_settings_get"); } catch (e) {}
  if (!config?.configured) {
    showAiSetupReminder();
    return;
  }
  closeActivityAi();
  const ov = document.createElement("div");
  ov.id = "activity-ai-overlay";
  ov.innerHTML = `<section><header><div><h2>AI 整理 · ${esc(digest.day)}</h2><p>这是预览，不会自动写入记忆。</p></div><button id="activity-ai-close" title="关闭"><img src="icons/x.svg" alt=""></button></header>
    <div class="activity-ai-note">单次仅发送对话标题、末条 Agent 回复、任务、产物路径和交接摘要，不发送完整对话、文件内容或 API Key。</div>
    <div id="activity-ai-result"><div class="state loading">正在发送精简工作记录并等待结果…</div></div></section>`;
  document.body.appendChild(ov);
  $("#activity-ai-close").onclick = closeActivityAi;
  ov.onclick = (e) => { if (e.target === ov) closeActivityAi(); };
  try {
    activityAiResponse = await invoke("ai_organize_day", { day: digest.day });
    if (!document.getElementById("activity-ai-result")) return;
    activitySaved.clear(); bindActivityAi(digest);
  } catch (e) {
    if ($("#activity-ai-result")) $("#activity-ai-result").innerHTML = `<div class="state error">AI 整理失败：${esc(String(e))}</div>`;
  }
}

function paintActivity() {
  const root = $("#page-activity");
  root.innerHTML = activityPageHtml(activityDigest, { ...activityState, today: localDay(new Date()) });
  root.querySelectorAll("[data-work-session]").forEach((b) => { b.onclick = () => showSession(b.dataset.workSession); });
  root.querySelectorAll("[data-activity-project]").forEach((b) => {
    b.onclick = () => { activityState.selected = b.dataset.activityProject; activityState.tab = "activities"; paintActivity(); };
  });
  root.querySelectorAll("[data-activity-tab]").forEach((b) => { b.onclick = () => { activityState.tab = b.dataset.activityTab; paintActivity(); }; });
  $("#activity-project-filter").onchange = (e) => {
    activityState.project = e.target.value; activityState.selected = e.target.value === "all" ? null : e.target.value; activityState.tab = "activities"; paintActivity();
  };
  $("#activity-agent-filter").onchange = (e) => { activityState.agent = e.target.value; activityState.selected = null; paintActivity(); };
  $("#activity-day").onchange = (e) => { if (e.target.value) { activityDay = e.target.value; activityDigest = null; pages.activity(); } };
  $("#activity-prev").onclick = () => { activityDay = shiftDay(activityDay, -1); activityDigest = null; pages.activity(); };
  $("#activity-next").onclick = () => { activityDay = shiftDay(activityDay, 1); activityDigest = null; pages.activity(); };
  $("#activity-export").onclick = async () => {
    const b = $("#activity-export"); b.disabled = true;
    try {
      const r = await invoke("daily_digest_export", { day: activityDigest.day });
      toast(`日报已导出：${r.path}`);
    } catch (e) { toast(String(e)); }
    finally { b.disabled = false; }
  };
  $("#activity-ai").onclick = () => showActivityAi(activityDigest);
}

async function renderActivity() {
  if (!activityDigest || activityDigest.day !== activityDay) activityDigest = await invoke("daily_digest", { day: activityDay });
  paintActivity();
}

function sessTable(sessions, opts = {}) {
  const { project: showProject = true, preview: showPreview = false, fixed = false,
          select = false, action = null, deleted = false } = opts;
  if (!sessions.length) return '<div class="empty">暂无对话</div>';
  const selHead = select
    ? '<th class="c-sel"><input type="checkbox" class="sel-all" title="全选/全不选当前列表" /></th>'
    : "";
  const selCell = (s) => (select ? `<td class="c-sel"><input type="checkbox" class="selbox" data-sid="${esc(s.session_id)}" /></td>` : "");
  const projHead = showProject ? "<th>项目</th>" : "";
  const projCell = (s) => (showProject ? `<td>${esc(s.project || "—")}</td>` : "");
  const prevHead = showPreview ? '<th class="c-prev">摘要</th>' : "";
  const prevCell = (s) => (showPreview ? `<td class="prev" title="${esc(s.preview || "")}">${esc(s.preview || "—")}</td>` : "");
  const delHead = deleted ? '<th class="c-time">删除于</th>' : "";
  const delCell = (s) => (deleted ? `<td class="c-time">${fmtTimeCell(s.deleted_at)}</td>` : "");
  const actHead = action ? '<th class="c-act"></th>' : "";
  const actCell = (s) => {
    if (action === "delete") return `<td class="c-act"><button class="btn small danger" data-del="${esc(s.session_id)}">删除</button></td>`;
    if (action === "restore") return `<td class="c-act"><button class="btn small" data-restore="${esc(s.session_id)}">恢复</button></td>`;
    if (action === "trash") return `<td class="c-act"><div class="action-buttons"><button class="btn small" data-restore="${esc(s.session_id)}">恢复</button>
      <button class="btn small danger" data-purge="${esc(s.session_id)}" title="${s.overdue ? "先归档备份再移除" : `保留期内（剩 ${s.remaining_days} 天），强制删除需确认；同样先归档备份`}">彻底删除</button></div></td>`;
    return "";
  };
  return `<table${fixed ? ` class="fixed${deleted ? " trash" : ""}"` : ""}><tr>${selHead}${prevHead}${projHead}<th class="c-agent">agent</th><th class="c-time">开始</th><th class="c-time">结束</th>${delHead}<th class="c-num">消息</th>${actHead}</tr>
    ${sessions.map((s) => `<tr class="clickable" data-sid="${esc(s.session_id)}">
      ${selCell(s)}${prevCell(s)}${projCell(s)}<td class="c-agent"><span class="pill ${esc(s.agent)}">${esc(s.agent)}</span></td>
      <td class="c-time">${fmtTimeCell(s.started_at)}</td><td class="c-time">${fmtTimeCell(s.ended_at)}</td>${delCell(s)}<td class="c-num">${s.messages}</td>${actCell(s)}</tr>`).join("")}
  </table>`;
}
function bindSessionRows(scope) {
  document.querySelectorAll(`${scope} tr[data-sid]`).forEach((tr) => {
    tr.onclick = () => showSession(tr.dataset.sid);
  });
}

// ---------------------------------------------------------------- projects
let showArchivedProjects = false;
// 展开全部 agent pill 的项目 id（跨重渲染保留，切页即重置）
const projPillsOpen = new Set();

const agentCell = (p) => {
  const list = p.agents || [];
  if (list.length === 0) return "";
  const pill = (a) => `<span class="pill ${esc(a)}">${esc(a)}</span>`;
  const chip = (n) => `<span class="pill more" data-expand="${p.id}" title="${esc(list.join(", "))}">+${n}</span>`;
  // 宽口径（≥1240px）：≤3 枚全显，更多收成 2 枚 + "+N"
  const wide = (list.length <= 3 || projPillsOpen.has(p.id))
    ? list.map(pill).join(" ") + (list.length > 3 ? ` <span class="pill more" data-collapse="${p.id}" title="收起">收起</span>` : "")
    : list.slice(0, 2).map(pill).join(" ") + " " + chip(list.length - 2);
  // 窄口径（<1240px）：1 枚 + "+N"，列宽 124 也单行；点击展开全部（允许折行）
  const narrow = projPillsOpen.has(p.id)
    ? list.map(pill).join(" ") + ` <span class="pill more" data-collapse="${p.id}" title="收起">收起</span>`
    : (list.length > 1 ? `${pill(list[0])} ${chip(list.length - 1)}` : pill(list[0]));
  return `<span class="ag-wide">${wide}</span><span class="ag-narrow">${narrow}</span>`;
};

async function renderProjects() {
  const d = await invoke("projects");
  $("#page-projects").innerHTML = `
    <h1>项目 <span class="en">Projects</span>
      <button class="btn small" id="proj-add-btn" style="margin-left:auto;align-self:center">添加项目</button>
    </h1>
    <div id="proj-add-row" class="inline-add-row hidden">
      <input type="text" id="proj-add-input" placeholder="粘贴项目文件夹的绝对路径（支持 ~）" />
      <button class="btn small" id="proj-add-pick">选择文件夹</button>
      <button class="btn primary small" id="proj-add-ok">添加</button>
      <button class="btn small" id="proj-add-cancel">取消</button>
    </div>
    <div id="projects-table"><table class="fixed"><tr><th class="c-name">项目</th><th>路径</th><th class="c-agents">agent</th><th class="c-num">对话</th><th class="c-num">消息</th><th class="c-time">最近活动</th><th class="c-time">最近 Handoff</th><th class="c-act"></th></tr>
    ${d.projects.map((p) => `<tr class="clickable" data-pid="${p.id}" data-path="${esc(p.path)}">
      <td class="c-name" title="${esc(p.name)}"><b>${esc(p.name)}</b></td><td title="${esc(p.path)}" style="color:var(--dim)">${esc(p.display_path || p.path)}</td>
      <td class="c-agents">${agentCell(p)}</td>
      <td class="c-num">${p.sessions}</td><td class="c-num">${p.messages}</td>
      <td class="c-time">${fmtTimeCell(p.last_activity)}</td><td class="c-time">${fmtTimeCell(p.last_handoff)}</td>
      <td class="c-act"><button class="btn small" data-arch="${p.id}">废弃</button></td></tr>`).join("")}
    </table></div>
    ${d.archived.length ? `
      <button class="chip" id="arch-toggle" style="margin-top:14px">${showArchivedProjects ? "隐藏已废弃项目" : `已废弃项目（${d.archived.length}）· 显示`}</button>
      ${showArchivedProjects ? `<div class="scrollbox" style="margin-top:10px"><table class="wrap">
        <tr><th>项目</th><th>路径</th><th class="c-num">对话</th><th>废弃于</th><th></th></tr>
        ${d.archived.map((p) => `<tr>
          <td><b>${esc(p.name)}</b></td><td style="color:var(--dim)">${esc(p.display_path || p.path)}</td>
          <td class="c-num">${p.sessions}</td><td style="color:var(--dim)">${fmtTime(p.archived_at)}</td>
          <td><button class="btn small" data-restore="${p.id}">恢复</button></td></tr>`).join("")}
      </table></div>` : ""}` : ""}
  `;
  // 行点击开卷宗；行内"废弃"是软归档（可恢复），不打断式确认
  document.querySelectorAll("#page-projects tr[data-pid]").forEach((tr) => {
    tr.onclick = () => showProject(+tr.dataset.pid);
  });
  document.querySelectorAll("#page-projects [data-expand]").forEach((b) => {
    b.onclick = (ev) => { ev.stopPropagation(); projPillsOpen.add(+b.dataset.expand); renderProjects(); };
  });
  document.querySelectorAll("#page-projects [data-collapse]").forEach((b) => {
    b.onclick = (ev) => { ev.stopPropagation(); projPillsOpen.delete(+b.dataset.collapse); renderProjects(); };
  });
  const addRow = $("#proj-add-row");
  $("#proj-add-btn").onclick = () => { addRow.classList.remove("hidden"); $("#proj-add-input").focus(); };
  const hideAdd = () => addRow.classList.add("hidden");
  $("#proj-add-cancel").onclick = hideAdd;
  bindFolderPicker("#proj-add-pick", "#proj-add-input", "#proj-add-ok");
  const submitAdd = async () => {
    const v = $("#proj-add-input").value.trim();
    if (!v) return;
    try {
      await invoke("project_add", { path: v });
      toast("已添加项目");
      renderProjects();
    } catch (e) { toast(String(e)); }
  };
  $("#proj-add-ok").onclick = submitAdd;
  $("#proj-add-input").onkeydown = (e) => {
    if (e.key === "Enter") submitAdd();
    if (e.key === "Escape") hideAdd();
  };
  document.querySelectorAll("#page-projects [data-arch]").forEach((b) => {
    b.onclick = async (ev) => {
      ev.stopPropagation();
      try {
        await invoke("project_archive", { id: +b.dataset.arch });
        toast("已废弃，可在下方恢复");
        renderProjects();
      } catch (e) { toast(String(e)); }
    };
  });
  const archToggle = $("#arch-toggle");
  if (archToggle) archToggle.onclick = () => { showArchivedProjects = !showArchivedProjects; renderProjects(); };
  document.querySelectorAll("#page-projects [data-restore]").forEach((b) => {
    b.onclick = async () => {
      try {
        await invoke("project_restore", { id: +b.dataset.restore });
        toast("已恢复");
        renderProjects();
      } catch (e) { toast(String(e)); }
    };
  });
  snapScrollboxTables($("#page-projects"));
}

async function showProject(pid) {
  const request = drawer.begin();
  let d;
  try { d = await invoke("project_dossier", { projectId: pid }); }
  catch (e) { if (drawer.isCurrent(request)) toast(`读取卷宗失败：${e}`); return; }
  if (!drawer.isCurrent(request)) return;
  const o = d.overview;
  // 决策板：活跃决策带头，superseded 折叠为演变史（治理层的独特价值）
  const decs = d.decisions || [];
  const prevOf = {};
  decs.forEach((m) => { if (m.superseded_by) (prevOf[m.superseded_by] ||= []).push(m); });
  const chained = new Set();
  const srcLink = (sid, line = null) => (sid
    ? `<span class="src" data-src="${esc(sid)}"${line != null ? ` data-line="${line}"` : ""}>来源${line != null ? ` #L${line}` : ""} ↗</span>`
    : "");
  const chainOf = (head) => {
    const out = [];
    const visited = new Set([head.id]);
    const queue = [...(prevOf[head.id] || [])];
    for (let i = 0; i < queue.length; i++) {
      const prev = queue[i];
      if (visited.has(prev.id)) continue; // 环防御：数据层已修，老库兜底
      visited.add(prev.id);
      chained.add(prev.id);
      out.push(`<div class="memcard superseded"><div class="content">↩ 被 [superseded] ${esc(prev.content)}</div>
        <div class="meta">${fmtTime(prev.created_at)} · id <code>${esc(prev.id)}</code> ${srcLink(prev.source_session_id, prev.source_line_no)}</div></div>`);
      queue.push(...(prevOf[prev.id] || []));
    }
    return out.join("");
  };
  const decisionRows = decs.filter((m) => m.status === "confirmed").map((m) => `
    <div class="memcard"><div class="content">${esc(m.content)}</div>
    <div class="meta"><span class="pill ${esc(m.status)}">${esc(m.status)}</span><span>${esc(m.type)}</span><span>${fmtTime(m.created_at)}</span>
    ${srcLink(m.source_session_id, m.source_line_no)}</div>${chainOf(m)}</div>`).join("");
  const orphanSuperseded = decs.filter((m) => m.status === "superseded" && !chained.has(m.id)).map((m) => `
    <div class="memcard superseded"><div class="content">${esc(m.content)}</div>
    <div class="meta"><span class="pill superseded">superseded</span>被 <code>${esc(m.superseded_by || "—")}</code> 取代 · ${fmtTime(m.created_at)} ${srcLink(m.source_session_id, m.source_line_no)}</div></div>`).join("");
  const timelineRows = (d.timeline || []).map((t) => `
    <tr class="clickable" data-sid="${esc(t.session_id)}">
      <td class="c-time">${fmtTime(t.started_at)}</td>
      <td class="c-agent"><span class="pill ${esc(t.agent)}">${esc(t.agent)}</span></td>
      <td>${t.link_type ? `<span class="pill" title="← ${esc(t.parent_session_id || "")}">${esc(t.link_type)}</span>` : ""}</td>
      <td class="c-num">${t.messages}</td>
      <td class="c-num">${t.artifacts || ""}</td></tr>`).join("");
  const artifactRows = (d.artifacts || []).map((a) => `
    <div class="memcard"><div class="content"><code data-path="${esc(a.path)}" style="cursor:context-menu">${esc(a.path)}</code></div>
    <div class="meta"><span>${esc(a.tool || "—")}</span> ${srcLink(a.session_id)}</div></div>`).join("");
  const handoffRows = (d.handoffs || []).map((h) => `
    <div class="memcard"><div class="content"><b>${esc(h.title)}</b>${h.next_steps ? `<br>下一步：${esc(h.next_steps)}` : ""}</div>
    <div class="meta">${fmtTime(h.created_at)} ${srcLink(h.session_id)}</div></div>`).join("");
  openDrawer(`
    <h1 style="font-size:16px">卷宗 · ${esc(d.project)}</h1>
    <div class="lineage">${esc(d.path)} · 活跃 ${fmtTime(o.first_activity)} → ${fmtTime(o.last_activity)} ·
      ${o.sessions} 对话 · ${o.messages} 消息 · ${o.agents} 种 agent</div>
    <h2>继续工作</h2><div id="project-continuation">加载中…</div>
    <h2>来源复查</h2><div class="memcard" id="project-review">检测中…</div>
    <h2>决策板（活跃 ${decs.filter((m) => m.status === "confirmed").length} / 历史 ${decs.length}）</h2>
    <div class="scrollbox">${decisionRows + orphanSuperseded || '<div class="empty">暂无决策/规则记忆</div>'}</div>
    <h2>时间线（${(d.timeline || []).length}）</h2>
    ${timelineRows ? `<div class="scrollbox"><table><tr><th class="c-time">开始</th><th class="c-agent">agent</th><th>链</th><th class="c-num">消息</th><th class="c-num">artifact</th></tr>${timelineRows}</table></div>` : '<div class="empty">暂无对话</div>'}
    ${lineageGraphHtml(
      (d.timeline || []).map((t) => ({ session_id: t.session_id, agent: t.agent, started_at: t.started_at, messages: t.messages, title: t.title })),
      d.lineage_edges || (d.timeline || []).filter((t) => t.link_type && t.parent_session_id).map((t) => ({ p: t.parent_session_id, c: t.session_id, lt: t.link_type }))
    ) || '<h2>对话谱系</h2><div class="empty">当前没有可追溯的对话继承关系。谱系图只连接由 fork、续聊、上下文压缩或子 agent 创建的对话；普通新聊天不会按时间自动连成树。</div>'}
    <h2>Artifacts（${(d.artifacts || []).length}）</h2>
    <div class="scrollbox">${artifactRows || '<div class="empty">暂无 artifact</div>'}</div>
    <h2>Handoff 链（${(d.handoffs || []).length}）</h2>
    <div class="scrollbox tight">${handoffRows || '<div class="empty">暂无 Handoff</div>'}</div>
  `, request);
  document.querySelectorAll("#drawer-content tr[data-sid]").forEach((tr) => {
    tr.onclick = () => showSession(tr.dataset.sid);
  });
  // 谱系图节点可点（跨项目父节点 .ext 只有 id，无数据不可点）+ 当前节点滚入视口
  afterGraphRender("#drawer-content");
  loadProjectRecall({ invoke, drawer, request, pid, $, esc, fmtTime, showSession, toast });
}
window.showSession = showSession;

// ---------------------------------------------------------------- sessions
let sessData = [];
let sessSel = null;
let sessAgent = "";
let sessTrash = false;
let sessChecked = new Set();
let sessShown = 200; // 列表分批渲染：已显示条数（数据全量持有，不封顶）
let sessGroup = "project"; // 可在项目视图与按月清理视图之间切换

// 二次确认按钮（借鉴本页 bundle 恢复的 armed 模式）：点第一次武装，
// 4 秒内点第二次才执行。
function armButton(btn, label, armedLabel, fn) {
  btn.dataset.armed = "";
  btn.textContent = label;
  btn.onclick = async () => {
    if (!btn.dataset.armed) {
      btn.dataset.armed = "1";
      btn.textContent = armedLabel;
      setTimeout(() => { if (btn.dataset.armed) { btn.dataset.armed = ""; btn.textContent = label; } }, 4000);
      return;
    }
    btn.dataset.armed = "";
    btn.textContent = label;
    try { await fn(); } catch (e) { toast(String(e)); }
  };
}

async function renderSessions() {
  sessChecked = new Set();
  sessShown = 200;
  if (sessTrash) return drawTrash();
  let d;
  try {
    d = await invoke("sessions", { projectId: null, limit: null });
  } catch (e) {
    $("#page-sessions").innerHTML = `<h1>对话 <span class="en">Sessions</span></h1><div class="empty">读取失败：${esc(String(e))}</div>`;
    return;
  }
  const groups = new Map();
  for (const s of d.sessions) {
    const date = s.ended_at || s.started_at || "";
    const key = sessGroup === "month"
      ? (date.slice(0, 7) || "（未记录时间）")
      : (s.project || "（无项目）");
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(s);
  }
  sessData = [...groups.entries()];
  if (!sessSel || !groups.has(sessSel)) sessSel = sessData[0]?.[0] ?? null;
  drawSessionsPage();
}

function drawSessionsPage() {
  const agents = AGENT_LIST;
  $("#page-sessions").innerHTML = `
    <h1>对话 <span class="en">Sessions</span></h1>
    <div class="sess-layout">
      <div class="sess-projs">
        ${sessData.map(([name, list]) => {
          const byAgent = {};
          list.forEach((s) => { byAgent[s.agent] = (byAgent[s.agent] || 0) + 1; });
          const agentStr = Object.entries(byAgent).map(([a, n]) => `${a}×${n}`).join(" ");
          const meta = sessGroup === "month"
            ? `${list.length} 个对话 · ${agentStr}`
            : `${list.length} 个对话 · ${agentStr} · ${fmtTime(list[0].ended_at || list[0].started_at)}`;
          return `
          <div class="sess-proj-item ${name === sessSel ? "active" : ""}" data-proj="${esc(name)}">
            <div class="n">${esc(name)}</div>
            <div class="c" title="${esc(meta)}">${esc(meta)}</div>
          </div>`;
        }).join("")}
      </div>
      <div class="sess-main">
        <div class="sess-filters">
          <button class="chip ${sessGroup === "project" ? "on" : ""}" id="sess-by-project">按项目</button>
          <button class="chip ${sessGroup === "month" ? "on" : ""}" id="sess-by-month">按月</button>
          ${agents.map((a) => `<button class="chip ${sessAgent === a ? "on" : ""}" data-agent="${a}">${a || "全部"}</button>`).join("")}
          <span style="flex:1"></span>
          <button class="chip danger hidden" id="sess-bulk-del"></button>
          <button class="chip" id="sess-trash-btn">回收站</button>
        </div>
        <div id="sess-table"></div>
      </div>
    </div>`;
  // 重画整页会清掉内部滚动位置：侧栏滚动任何交互都保留（点下部项目不能跳顶，
  // 真机反馈 2026-09-04）；表格滚动仅在「显示更多」续排时保留（上方内容不变，
  // 同一偏移即同一视野），切项目/agent 回顶看新选择的第一屏。
  document.querySelectorAll(".sess-proj-item").forEach((el) => {
    el.onclick = () => {
      const side = document.querySelector(".sess-projs")?.scrollTop || 0;
      sessSel = el.dataset.proj; sessChecked = new Set(); sessShown = 200; drawSessionsPage();
      const pj = document.querySelector(".sess-projs"); if (pj) pj.scrollTop = side;
    };
  });
  document.querySelectorAll("#page-sessions .chip[data-agent]").forEach((el) => {
    el.onclick = () => {
      const side = document.querySelector(".sess-projs")?.scrollTop || 0;
      sessAgent = el.dataset.agent; sessShown = 200; drawSessionsPage();
      const pj = document.querySelector(".sess-projs"); if (pj) pj.scrollTop = side;
    };
  });
  const switchGroup = (group) => {
    if (sessGroup === group) return;
    sessGroup = group; sessSel = null; sessChecked = new Set(); sessShown = 200;
    renderSessions();
  };
  $("#sess-by-project").onclick = () => switchGroup("project");
  $("#sess-by-month").onclick = () => switchGroup("month");
  $("#sess-trash-btn").onclick = () => { sessTrash = true; sessChecked = new Set(); drawTrash(); };
  const all = (sessData.find(([n]) => n === sessSel)?.[1] || []).filter((s) => !sessAgent || s.agent === sessAgent);
  // 全量数据分批渲染：首屏 200，「显示更多」续排——资产不封顶，DOM 不拖垮
  const shown = all.slice(0, sessShown);
  $("#sess-table").innerHTML = sessTable(shown, { project: sessGroup === "month", preview: true, fixed: true, select: true, action: "delete" })
    + (all.length > shown.length
      ? `<div style="padding:10px 0;text-align:center"><button class="chip" id="sess-more">显示更多（还有 ${all.length - shown.length} 个）</button></div>`
      : "");
  $("#sess-more")?.addEventListener("click", () => {
    const side = document.querySelector(".sess-projs")?.scrollTop || 0;
    const tbl = document.querySelector("#sess-table")?.scrollTop || 0;
    sessShown += 200; drawSessionsPage();
    const pj = document.querySelector(".sess-projs"); if (pj) pj.scrollTop = side;
    const tb = document.querySelector("#sess-table"); if (tb) tb.scrollTop = tbl;
  });
  bindSessionRows("#page-sessions");

  // 批量选择：勾选出现批量删除按钮，二次确认执行
  const bulk = $("#sess-bulk-del");
  const syncBulk = () => {
    bulk.classList.toggle("hidden", sessChecked.size === 0);
    if (!bulk.dataset.armed) bulk.textContent = `删除选中（${sessChecked.size}）`;
  };
  document.querySelectorAll("#page-sessions .selbox").forEach((box) => {
    box.checked = sessChecked.has(box.dataset.sid);
    box.onclick = (e) => {
      e.stopPropagation();
      box.checked ? sessChecked.add(box.dataset.sid) : sessChecked.delete(box.dataset.sid);
      syncBulk();
    };
  });
  // 全选当前列表（过滤后的可见集合；分批渲染下作用于全量数据，勾选不受批次影响）
  const selAll = document.querySelector("#page-sessions .sel-all");
  if (selAll) {
    selAll.checked = all.length > 0 && all.every((s) => sessChecked.has(s.session_id));
    selAll.onclick = (e) => {
      e.stopPropagation();
      if (selAll.checked) all.forEach((s) => sessChecked.add(s.session_id));
      else all.forEach((s) => sessChecked.delete(s.session_id));
      drawSessionsPage();
    };
  }
  syncBulk();
  armButton(bulk, `删除选中（${sessChecked.size}）`, "确认删除？进回收站，可恢复", async () => {
    for (const sid of sessChecked) await invoke("session_delete", { sessionId: sid });
    toast(`已删除 ${sessChecked.size} 个对话（回收站可恢复）`);
    renderSessions();
  });
  document.querySelectorAll("#page-sessions [data-del]").forEach((btn) => {
    btn.addEventListener("click", (e) => e.stopPropagation());
    armButton(btn, "删除", "确认删除？", async () => {
      await invoke("session_delete", { sessionId: btn.dataset.del });
      toast("已删除（回收站可恢复）");
      renderSessions();
    });
  });
}

// 回收站（DESIGN-0.3 §6 软删）：恢复 / 彻底删除（归档式 purge，保留期 30 天内
// 拒绝）；"清空超期"与 CLI session empty 同一预览→执行语义。
async function drawTrash() {
  let d;
  try { d = await invoke("trash_list"); } catch (e) { toast(String(e)); return; }
  const overdueCount = d.trash.filter((s) => s.overdue).length;
  $("#page-sessions").innerHTML = `
    <h1>回收站 <span class="en">Trash</span></h1>
    <div class="sess-filters">
      <button class="chip" id="trash-back">← 返回对话列表</button>
      <span style="flex:1"></span>
      <button class="chip danger hidden" id="trash-bulk"></button>
      <button class="chip danger hidden" id="trash-bulk-purge"></button>
      <button class="chip danger ${overdueCount ? "" : "hidden"}" id="trash-empty">清空超期（${overdueCount}）</button>
    </div>
    <div class="sess-meta" style="color:var(--dim);margin-bottom:12px">软删对话 ${d.trash.length} 个 · 数据完整保留，恢复后可检索 · 彻底删除 = 对话原件一并移除，不可恢复（保留期 30 天，期内删除需确认）</div>
    <div id="trash-table"></div>`;
  $("#trash-back").onclick = () => { sessTrash = false; renderSessions(); };
  $("#trash-table").innerHTML = d.trash.length
    ? sessTable(d.trash, { project: true, fixed: true, select: true, action: "trash", deleted: true })
    : '<div class="empty">回收站为空</div>';
  const bulk = $("#trash-bulk");
  const syncBulk = () => {
    bulk.classList.toggle("hidden", sessChecked.size === 0);
    if (!bulk.dataset.armed) bulk.textContent = `恢复选中（${sessChecked.size}）`;
  };
  document.querySelectorAll("#page-sessions .selbox").forEach((box) => {
    box.checked = sessChecked.has(box.dataset.sid);
    box.onclick = (e) => {
      e.stopPropagation();
      box.checked ? sessChecked.add(box.dataset.sid) : sessChecked.delete(box.dataset.sid);
      syncBulk();
      syncBulkPurge();
    };
  });
  const selAll = document.querySelector("#page-sessions .sel-all");
  if (selAll) {
    selAll.checked = d.trash.length > 0 && d.trash.every((s) => sessChecked.has(s.session_id));
    selAll.onclick = (e) => {
      e.stopPropagation();
      if (selAll.checked) d.trash.forEach((s) => sessChecked.add(s.session_id));
      else d.trash.forEach((s) => sessChecked.delete(s.session_id));
      drawTrash();
    };
  }
  syncBulk();
  armButton(bulk, `恢复选中（${sessChecked.size}）`, "确认恢复？", async () => {
    for (const sid of sessChecked) await invoke("session_restore", { sessionId: sid });
    toast(`已恢复 ${sessChecked.size} 个对话`);
    sessChecked = new Set();
    drawTrash();
  });
  // 批量彻底删除选中：保留期内的走强制通道；档案取舍二段确认（默认保留档案）
  const bulkPurge = $("#trash-bulk-purge");
  const syncBulkPurge = () => {
    bulkPurge.classList.toggle("hidden", sessChecked.size === 0);
    if (!bulkPurge.dataset.armed) bulkPurge.textContent = `彻底删除选中（${sessChecked.size}）`;
  };
  syncBulkPurge();
  armButton(bulkPurge, `彻底删除选中（${sessChecked.size}）`, "确认彻底删除？不可恢复", async () => {
    const ids = [...sessChecked];
    const inRetention = d.trash.filter((s) => ids.includes(s.session_id) && !s.overdue).length;
    if (inRetention > 0 && !confirm(`选中有 ${inRetention} 个还在 30 天保留期内，将强制彻底删除。继续？`)) return;
    const r = await invoke("trash_purge_selected", { ids, force: inRetention > 0 });
    const failCount = (r.failed || []).length;
    toast(failCount ? `已彻底删除 ${r.purged} 个，${failCount} 个失败` : `已彻底删除 ${r.purged} 个对话`);
    sessChecked = new Set();
    drawTrash();
  });
  document.querySelectorAll("#page-sessions [data-purge]").forEach((btn) => {
    btn.addEventListener("click", (e) => e.stopPropagation());
    armButton(btn, "彻底删除", "确认彻底删除？不可恢复", async () => {
      const item = d.trash.find((s) => s.session_id === btn.dataset.purge);
      if (item && !item.overdue && !confirm("该对话还在 30 天保留期内，将强制彻底删除。继续？")) return;
      await invoke("trash_purge", { sessionId: btn.dataset.purge, force: !!(item && !item.overdue) });
      toast("已彻底删除");
      sessChecked.delete(btn.dataset.purge);
      drawTrash();
    });
  });
  // 清空超期（超过 30 天保留期）：预览数字 → armed 二次确认 → 执行
  const emptyBtn = $("#trash-empty");
  if (emptyBtn) {
    armButton(emptyBtn, `清空超期（${overdueCount}）`, `确认彻底删除 ${overdueCount} 个超期对话？不可恢复`, async () => {
      const r = await invoke("trash_empty_overdue");
      toast(`已彻底删除 ${r.purged_sessions} 个超期对话（对象已归档备份）`);
      sessChecked = new Set();
      drawTrash();
    });
  }
  document.querySelectorAll("#page-sessions [data-restore]").forEach((btn) => {
    btn.addEventListener("click", (e) => e.stopPropagation());
    armButton(btn, "恢复", "确认恢复？", async () => {
      await invoke("session_restore", { sessionId: btn.dataset.restore });
      toast("已恢复");
      sessChecked.delete(btn.dataset.restore);
      drawTrash();
    });
  });
}

// ---------------------------------------------------------------- memory
let memStatus = "";
let memAgent = "";
let memType = "";
let memView = "list";
const AGENT_LIST = ["", "claude", "codex", "opencode", "zcode", "kimi", "hermes"];
const shortId = (sid) => (sid ? sid.split(":")[1] || sid : "");

let memoryRequest = 0;
async function renderMemory() {
  const request = ++memoryRequest;
  // 关系图需要全状态（superseded 在旧记忆上、confirmed 在链头——单状态断链）
  // 图模式 limit=0（无上限，后端不拼 LIMIT）：任何硬上限都会按 updated_at
  // DESC 截掉最旧链尾、连线静默断裂（codex 二/三审）。记忆数量级百到千。
  const statusArg = memView === "graph" ? "all" : memStatus || null;
  let d, sg, nf;
  try {
    [d, sg, nf] = await Promise.all([
      invoke("memories", { status: statusArg, agent: memView === "graph" ? null : memAgent || null, type: memView === "graph" ? null : memType || null, limit: memView === "graph" ? 0 : 200 }),
      invoke("memories", { status: "suggested", limit: 21 }).catch(() => null),
      invoke("memory_files"),
    ]);
  } catch (e) { if (request === memoryRequest) toast(`读取记忆失败：${e}`); return; }
  if (request !== memoryRequest) return;
  const suggestedCount = sg ? sg.memories.length : 0;
  const counts = d.memories.length;
  $("#page-memory").innerHTML = `
    <h1>记忆 <span class="en">Memory</span></h1>
    <details class="memcard"><summary>新增记忆模板</summary><div class="content">用于把可复用的经验、决策或偏好保存为一条记忆；它不会从对话中自动生成。按实际证据填写，未验证内容标为待验证，再通过 agent 的 save_memory 或 CLI memory add 保存。</div><pre>${esc(EXPERIENCE_TEMPLATE)}</pre><button class="btn small" data-copy="${esc(EXPERIENCE_TEMPLATE)}">复制模板</button></details>
    <div class="searchbar">
      <select id="mem-status">
        ${memView === "graph" ? '<option value="all">全部状态（关系图）</option>' : ""}
        <option value="">活跃（suggested + confirmed）</option>
        <option value="suggested">待确认 suggested</option>
        <option value="confirmed">已确认 confirmed</option>
        <option value="superseded">已取代 superseded</option>
        <option value="archived">已归档 archived</option>
      </select>
      <select id="mem-type">
        <option value="">全部类型</option>
        <option value="task">任务 task</option>
        <option value="decision">决策 decision</option>
        <option value="rule">规则 rule</option>
        <option value="lesson">教训 lesson</option>
        <option value="preference">偏好 preference</option>
        <option value="fact">事实 fact</option>
        <option value="context">上下文 context</option>
      </select>
      <span style="flex:1"></span>
      <button class="chip ${memView === "graph" ? "on" : ""}" id="mem-graph-toggle">关系图</button>
    </div>
    <div class="pillrow" style="margin-bottom:10px">
      ${AGENT_LIST.map((a) => `<button class="chip ${(memView === "graph" ? "" : memAgent) === a ? "on" : ""}" data-mem-agent="${a}">${a || "全部来源"}</button>`).join("")}
    </div>
    <div style="color:var(--dim);margin-bottom:10px">${counts} 条${memView === "graph" ? " · 显示全部来源与类型，点节点高亮演变链" : ""}</div>
    ${suggestedCount >= 21 ? `<div class="digest digest-error">待确认记忆积压 20+ 条（容量纪律：先处理积压，再新增）</div>` : ""}
    ${memView === "graph" ? memoryGraphHtml(d.memories) : `<div class="scrollbox">${d.memories.map(memCard).join("") || '<div class="empty">暂无记忆</div>'}</div>`}
    <h2 style="margin-top:24px">原生 memory 备份</h2>
    <div style="color:var(--dim);margin-bottom:8px">agent 自己的 memory 文件（MEMORY.md / AGENTS.md）的整文件快照与修订历史——只读备份，独立于上方治理记忆</div>
    ${nf.memory_files.map((f) => `
      <div class="memcard clickable" data-mfid="${f.id}">
        <div class="content" style="word-break:break-all">${esc(f.path)}</div>
        <div class="meta"><span class="pill ${esc(f.agent)}">${esc(f.agent)}</span>
          <span>${esc(f.scope)}</span><span>${f.revisions} 个修订</span><span>${fmtTime(f.last_captured)}</span></div>
      </div>`).join("") || '<div class="empty">暂无（执行 import 后显示）</div>'}`;
  $("#mem-status").value = memView === "graph" ? "all" : memStatus;
  $("#mem-type").value = memView === "graph" ? "" : memType;
  document.querySelectorAll("#page-memory [data-mem-agent]").forEach((el) => {
    el.disabled = memView === "graph";
    el.onclick = () => { memAgent = el.dataset.memAgent; renderMemory(); };
  });
  const gt = $("#mem-graph-toggle");
  gt.onclick = () => { memView = memView === "graph" ? "list" : "graph"; renderMemory(); };
  $("#mem-status").disabled = memView === "graph";
  $("#mem-type").disabled = memView === "graph";
  if (memView === "graph") bindMemoryGraph(d.memories);
  $("#mem-status").onchange = (e) => { memStatus = e.target.value; renderMemory(); };
  $("#mem-type").onchange = (e) => { memType = e.target.value; renderMemory(); };
  document.querySelectorAll("#page-memory [data-act]").forEach((btn) => {
    btn.onclick = async () => {
      await invoke("update_memory", { id: btn.dataset.id, action: btn.dataset.act, supersededBy: null });
      toast(btn.dataset.act === "confirm" ? "已确认" : "已归档");
      renderMemory();
    };
  });
  // 取代：内联搜索选择取代者（webview 不支持 window.prompt）。候选 = 全部活跃
  // 记忆（不含自己），按内容/id 过滤；也接受直接粘贴完整 id。提交前统一在
  // 清单里选定；后端再次校验目标与环，防止过期候选破坏关系。
  document.querySelectorAll("#page-memory [data-supersede]").forEach((btn) => {
    btn.onclick = async () => {
      const id = btn.dataset.supersede;
      let candidates;
      try {
        const r = await invoke("memories", { status: null, agent: null, type: null, limit: 0 });
        candidates = r.memories.filter((m) => m.id !== id);
      } catch (e) { toast(String(e)); return; }
      const span = document.createElement("span");
      span.className = "supersede-box";
      span.innerHTML = `<input type="text" class="filter-input" style="padding:3px 8px;font-size:12px;min-width:230px" placeholder="输入内容关键词，或粘贴完整 ID" />
        <button class="btn small primary" disabled>确定</button>
        <button class="btn small">取消</button>`;
      const dropdown = document.createElement("div");
      dropdown.className = "pick-list hidden";
      span.appendChild(dropdown);
      btn.replaceWith(span);
      const input = span.querySelector("input");
      const [okBtn, cancelBtn] = span.querySelectorAll("button");
      input.focus();
      let picked = null;
      const setPicked = (m) => {
        picked = m;
        okBtn.disabled = false;
        const c = m.content;
        input.value = m.id;
        dropdown.classList.add("hidden");
        let hint = span.querySelector(".pick-chosen");
        if (!hint) {
          hint = document.createElement("span");
          hint.className = "pick-chosen";
          span.appendChild(hint);
        }
        hint.textContent = `取代者：${c.slice(0, 40)}${c.length > 40 ? "…" : ""}`;
        hint.title = c;
      };
      const clearPicked = () => {
        picked = null;
        okBtn.disabled = true;
        const hint = span.querySelector(".pick-chosen");
        if (hint) hint.remove();
      };
      cancelBtn.onclick = () => renderMemory();
      // 输入解析：完整 id 命中 → 直接选定；否则返回候选列表（null = 空输入）
      const resolve = (raw) => {
        const q = raw.trim().toLowerCase();
        if (!q) { clearPicked(); dropdown.classList.add("hidden"); return null; }
        const exact = candidates.find((m) => m.id.toLowerCase() === q);
        if (exact) { setPicked(exact); return null; }
        return candidates
          .filter((m) => m.content.toLowerCase().includes(q) || m.id.toLowerCase().includes(q))
          .slice(0, 6);
      };
      const renderHits = (hits) => {
        if (!hits) { dropdown.classList.add("hidden"); return; }
        dropdown.innerHTML = hits.length
          ? hits.map((m) => `<div class="pick-row" data-pid="${esc(m.id)}"><span class="pill">${esc(m.type)}</span> ${esc(m.content.slice(0, 60))}${m.content.length > 60 ? "…" : ""} <span class="pr-meta">${esc(m.project || m.scope)}</span></div>`).join("")
          : '<div class="pick-hint">无匹配的活跃记忆——更换关键词，或直接粘贴完整 ID</div>';
        dropdown.classList.remove("hidden");
        dropdown.querySelectorAll(".pick-row").forEach((row) => {
          row.onclick = () => setPicked(candidates.find((m) => m.id === row.dataset.pid));
        });
      };
      input.oninput = () => {
        clearPicked();
        renderHits(resolve(input.value));
      };
      const submit = async () => {
        if (!picked) {
          const hits = resolve(input.value);
          if (Array.isArray(hits) && hits.length === 1) setPicked(hits[0]);
          else { renderHits(hits); return; }
        }
        try {
          await invoke("update_memory", { id, action: "supersede", supersededBy: picked.id });
          toast("已标记为被取代");
          renderMemory();
        } catch (e) { toast(String(e)); }
      };
      okBtn.onclick = submit;
      input.onkeydown = (e) => {
        if (e.key === "Enter") submit();
        if (e.key === "Escape") cancelBtn.onclick();
      };
    };
  });
  document.querySelectorAll("#page-memory .memcard [data-src]").forEach((el) => {
    el.onclick = () => showSession(el.dataset.src, el.dataset.line == null ? null : Number(el.dataset.line));
  });
  document.querySelectorAll("#page-memory [data-mfid]").forEach((el) => {
    el.onclick = () => showMemoryFile(+el.dataset.mfid);
  });
  bindCopyButtons("#page-memory"); // 记忆 id chip：点击复制完整 id
}

const memCard = (m) => {
  const active = m.status === "suggested" || m.status === "confirmed";
  return `
  <div class="memcard" data-mid="${esc(m.id)}">
    <div class="content">${esc(m.content)}</div>
    <div class="meta">
      <span class="pill">${esc(m.type)}</span>
      <span class="pill ${esc(m.status)}">${esc(m.status)}</span>
      ${m.source_agent ? `<span class="pill ${esc(m.source_agent)}">${esc(m.source_agent)}</span>` : ""}
      <span>${esc(m.scope)}${m.project ? " · " + esc(m.project) : ""}</span>
      <span>${fmtTime(m.updated_at)}</span>
      ${m.superseded_by ? `<span title="${esc(m.superseded_by)}">被 ${esc(m.superseded_by.slice(0, 12))}… 取代</span>` : ""}
      <code class="mid" data-copy="${esc(m.id)}" title="记忆 ID：${esc(m.id)}（点击复制）·「取代」其他记忆时填入">${esc(m.id.slice(0, 10))}</code>
      ${m.source_session_id ? `<span data-src="${esc(m.source_session_id)}"${m.source_line_no != null ? ` data-line="${m.source_line_no}"` : ""} style="cursor:pointer;color:var(--accent)">来源 ↗</span>` : '<span>来源未指定</span>'}
      ${m.status === "suggested" ? `<button class="btn small primary" data-act="confirm" data-id="${esc(m.id)}">确认</button>` : ""}
      ${active ? `<button class="btn small" data-supersede="${esc(m.id)}" title="取代：标记此记忆已被更新的记忆替代——选择取代者（搜索内容或粘贴 ID），形成可追溯的演变链">取代</button>` : ""}
      ${active ? `<button class="btn small" data-act="archive" data-id="${esc(m.id)}">归档</button>` : ""}
    </div>
  </div>`;
};

// ---------------------------------------------------------------- settings
// 设置页二级 tab（0.4.2 用户反馈：单页太长）——模块级状态，操作后 re-render 不丢当前 tab
let showDisabledSources = false;
let showWatchlist = false;
let settingsTab = "sources";
const EXPERIENCE_TEMPLATE = `问题：
适用条件：
已尝试但失败的方法：
Why（原因）：
How to apply（做法）：
验证方式与实际结果：
来源：session_id / message_id 或文件与版本
复查条件：`;

async function renderSettings() {
  const info = await invoke("app_info");
  const agentsInfo = await invoke("agents_detect");
  const autoPurge = (await invoke("auto_purge_get")).auto_purge_trash;
  const cap = await invoke("capability_matrix");
  const idx = await invoke("index_status");
  const bd = await invoke("backup_dir_get");
  let ai = { base_url: "https://api.openai.com/v1", model: "", max_input_chars: 40000, key_configured: false, key_source: null };
  let aiSettingsError = "";
  try { ai = await invoke("ai_settings_get"); } catch (e) { aiSettingsError = String(e); }
  // 能力矩阵格子：✓ 支持 / ◐ 部分 / — 不支持（诚实自报，证据写在 adapter 注释里）
  const capCell = (v) => v === "yes" ? '<span class="cap-yes">✓</span>'
    : v === "partial" ? '<span class="cap-partial">◐</span>'
    : '<span class="cap-no">—</span>';
  const home = info.home;
  const setupAgentNames = ["claude", "codex", "zcode", "kimi", "gemini", "cursor", "hermes"];
  const setupAgents = setupAgentNames.map((name) => {
    const source = agentsInfo.agents.find((a) => a.agent === name);
    const watched = agentsInfo.watchlist.find((a) => a.agent === name);
    const detected = !!source?.detected || !!watched?.detected;
    return { name, checked: detected && !source?.disabled, disabled: !!source?.disabled };
  });
  const ingestAgentNames = agentsInfo.agents.map((a) => a.agent);
  const watchedHere = agentsInfo.watchlist
    .filter((a) => a.detected && !setupAgentNames.includes(a.agent))
    .map((a) => a.agent);
  // 简约优先（用户反馈 2026-08-29）：已停用的源与未安装的观察名单默认都收起来
  const disabledCount = agentsInfo.agents.filter((a) => a.disabled).length;
  // 默认文件名用本地日期：toISOString 是 UTC，东八区凌晨 0-8 点会早一天（自检 C5）
  const now = new Date();
  const pad = (n) => String(n).padStart(2, "0");
  const today = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}`;
  // 二级 tab：数据源 / 能力矩阵 / 存储与备份 / 接入 / AI 整理 / 通用
  const stab = (id, label) => `<button class="subtab ${settingsTab === id ? "on" : ""}" data-stab="${id}">${label}</button>`;
  const panel = (id, inner) => `<div class="subpanel ${settingsTab === id ? "on" : ""}" data-spanel="${id}">${inner}</div>`;
  $("#page-settings").innerHTML = `
    <h1>设置 <span class="en">Settings</span></h1>
    <div class="subtabs">
      ${stab("sources", "数据源")}${stab("cap", "能力矩阵")}${stab("backup", "存储与备份")}${stab("setup", "接入")}${stab("ai", "AI 整理")}${stab("general", "通用")}
    </div>
    ${panel("sources", `
      <h2>Agent 数据源</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">文件型 agent 可登记额外采集根（import 时自动合并；opencode 是单库源，不支持）。卸载 agent 后在此停用：不再采集，历史数据仍可检索。</div>
        <div class="scrollbox"><table class="wrap"><tr><th>agent</th><th>默认根</th><th>检测</th><th class="c-num">对话</th><th>最后采集</th><th>额外根</th><th>采集</th></tr>
        ${agentsInfo.agents.filter((a) => showDisabledSources || !a.disabled).map((a) => `<tr style="${a.disabled ? "opacity:.5" : ""}">
          <td><span class="pill ${esc(a.agent)}">${esc(a.agent)}</span></td>
          <td style="color:var(--dim);word-break:break-all">${esc(a.root)}</td>
          <td>${a.detected ? "✓ 已检测到" : '<span style="color:var(--faint)">未发现</span>'}</td>
          <td class="c-num">${a.sessions}</td>
          <td style="color:var(--dim);white-space:nowrap">${a.last_imported ? fmtTime(a.last_imported) : "—"}</td>
          <td>${(a.extra_roots || []).map((r) => `<div style="word-break:break-all">${esc(r)} <button class="btn small" data-rmroot="${esc(a.agent)}|${esc(r)}">移除</button></div>`).join("") || ""}</td>
          <td><label style="display:flex;align-items:center;gap:6px;cursor:pointer;white-space:nowrap">
            <input type="checkbox" data-agent-toggle="${esc(a.agent)}" ${a.disabled ? "" : "checked"} />
            <span style="font-size:12px;color:var(--dim)">${a.disabled ? "已停用" : "采集中"}</span>
          </label></td>
        </tr>`).join("")}</table></div>
        ${disabledCount ? `<button class="chip" id="toggle-disabled" style="margin-top:8px">${showDisabledSources ? "隐藏已停用的源" : `已停用的源（${disabledCount}）· 显示`}</button>` : ""}
        <div class="searchbar" style="margin-top:8px">
          <select id="agent-add-name">
            <option value="claude">claude</option><option value="codex">codex</option>
            <option value="zcode">zcode</option><option value="kimi">kimi</option>
          </select>
          <input type="text" id="agent-add-path" style="flex:1" placeholder="额外采集根目录（绝对路径，必须已存在）" />
          <button class="btn" id="agent-add-pick">选择文件夹</button>
          <button class="btn" id="agent-add-btn">添加</button>
        </div>
        <button class="chip" id="toggle-watchlist" style="margin-top:10px">${showWatchlist ? "收起其他 agent 检测" : "展开其他主流 agent 检测"}</button>
        ${showWatchlist && (agentsInfo.watchlist || []).length ? `
        <div class="meta" style="margin-top:10px">目录级检测：解析需专属适配器，逐个支持；检测到目录仅表示本机安装过该 agent。</div>
        <div class="pillrow" style="margin-top:8px">
          ${agentsInfo.watchlist.map((w) => `<span class="chip" title="${esc(w.root)}">${esc(w.agent)} ${w.detected ? "· 已安装" : "· 未发现"}</span>`).join("")}
        </div>` : ""}
      </div>`)}

    ${panel("cap", `
      <h2>能力矩阵</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">✓ 支持 · ◐ 部分 · — 不支持。写回仅限文件型 agent（SQLite 型不写他人库）；源加密为硬阻断。</div>
        <table><tr><th>agent</th><th>采集</th><th>搜索</th><th>谱系</th><th>resume</th><th>写回</th><th>源加密</th><th>备注</th></tr>
        ${cap.adapters.map((a) => `<tr>
          <td><span class="pill ${esc(a.agent)}">${esc(a.agent)}</span></td>
          <td>${capCell(a.transcript)}</td><td>${capCell(a.search)}</td><td>${capCell(a.lineage)}</td>
          <td>${capCell(a.resume)}</td><td>${capCell(a.writeback)}</td><td>${capCell(a.encrypted)}</td>
          <td style="color:var(--dim);font-size:12px">${esc(a.notes)}</td></tr>`).join("")}
        ${cap.encrypted_watchlist.map((w) => `<tr style="opacity:.55">
          <td><span class="pill">${esc(w.agent)}</span></td>
          <td colspan="5" style="color:var(--faint)">未接入</td>
          <td><span class="cap-no">加密</span></td>
          <td style="color:var(--dim);font-size:12px">${esc(w.notes)}</td></tr>`).join("")}
        </table>
      </div>`)}

    ${panel("backup", `
      <h2>备份与导出位置</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">这里只存放手动生成的数据库快照、会话导出和删除档案。保存新位置时会迁移这些已有文件，不会移动软件正在使用的数据库和会话原文归档。</div>
        <div class="searchbar">
          <input type="text" id="backup-dir" style="flex:1" placeholder="绝对路径，如 D:\\yourmem-backup" value="${esc(bd.configured)}" />
          <button class="btn" id="backup-dir-pick">选择文件夹</button>
          <button class="btn primary" id="backup-dir-save">保存</button>
        </div>
        <div id="backup-dir-report" class="meta" style="margin-top:8px">当前：${esc(bd.effective)}</div>
      </div>
      <h2>存储占用</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">核心数据是软件正在使用的数据；备份与导出是按需生成的文件，两者不是两份重复备份。</div>
        <div class="searchbar" style="margin-top:0">
          <button class="btn" id="storage-usage">查看占用</button>
          <button class="btn" id="storage-compact">回收空闲空间</button>
        </div>
        <div id="storage-report"></div>
      </div>
      <h2>搜索索引范围</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">默认只索引对话内容，工具输出可按短词检索，数据库体积可控。开启全文索引后，工具输出全部可搜，数据库体积明显增大，重建需数分钟。</div>
        <div class="searchbar">
          <label style="display:flex;align-items:center;gap:8px;cursor:pointer">
            <input type="checkbox" id="tools-index" ${idx.tool_index_full ? "checked" : ""} />
            <span>全文索引工具输出</span>
          </label>
          <span id="tools-index-state" style="color:var(--faint);font-size:12px">已索引 ${idx.indexed_tool_rows} / ${idx.tool_messages} 行</span>
        </div>
      </div>
      ${snapshotPanelHtml()}
      <h2>完整备份与恢复</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">完整备份包含数据库和引用的原始记录，保存为 .tar.gz；单独的数据库快照不足以恢复原件。</div>
        <div class="searchbar">
          <input type="text" id="bundle-out" style="flex:1" value="${esc(bd.effective)}/backup-${today}.tar.gz" />
          <button class="btn" id="bundle-out-pick">选择位置</button>
          <button class="btn primary" id="bundle-create">创建备份</button>
        </div>
        <div class="searchbar">
          <input type="text" id="bundle-path" style="flex:1" placeholder="备份路径（.tar.gz 文件）" />
          <button class="btn" id="bundle-path-pick">选择备份</button>
          <button class="btn" id="bundle-verify">校验</button>
          <button class="btn danger" id="bundle-restore">合并恢复</button>
        </div>
        <div id="bundle-report" role="status" aria-live="polite" style="line-height:1.6;overflow-wrap:anywhere"></div>
      </div>
      <h2>彻底删除的离线档案</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">「彻底删除」已不再保留原件；此处管理的是历史版本或 CLI --keep-archive 产生的档案，删除后对应字节不再存在于磁盘。</div>
        <div class="searchbar">
          <button class="btn" id="archives-list">查看占用</button>
          <button class="btn danger hidden" id="archives-clear">清空全部</button>
        </div>
        <div id="archives-report"></div>
      </div>`)}

    ${panel("setup", `
      <h2>调用状态</h2>
      <div class="memcard"><div class="meta">配置登记请点击下方检测。这里显示本机最近完成的调用；成功表示工具返回成功，不代表答案已被采用。MCP 记录不区分客户端。</div>
      <table><tr><th>入口 / 操作</th><th>最近结果</th><th>完成时间</th><th>上次成功</th></tr>${(info.recall_status || []).map(r => `<tr><td>${esc(r.source)} / ${esc(r.name)}</td><td>${r.ok === true ? `成功${r.result_count != null ? `（${r.result_count} 条）` : ""}` : r.ok === false ? "失败" : "暂无结果记录"}</td><td>${fmtTime(r.completed_at)}</td><td>${fmtTime(r.last_success)}</td></tr>`).join("")}</table></div>
      <h2>一键接入 agent</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">一键接入表示自动修改 agent 的 MCP 配置；它与会话采集是两项不同能力。是否正在运行不影响检测结果，新配置在新会话中生效；覆盖前自动 .bak 备份。</div>
        <div class="setup-scope">
          <div><strong>自动配置 MCP</strong><span>${setupAgentNames.join("、")}</span></div>
          <div><strong>支持会话采集</strong><span>${ingestAgentNames.join("、")}</span></div>
          ${watchedHere.length ? `<div><strong>本机仅检测到目录</strong><span>${watchedHere.join("、")}（尚未接入自动配置和会话采集）</span></div>` : ""}
          <div><strong>其他 MCP agent</strong><span>兼容 stdio MCP 的客户端可手动配置 <code>yourmem mcp</code>，不受上列名单限制</span></div>
        </div>
        <div class="pillrow" id="setup-agents">
          ${setupAgents.map((a) => `<label class="chip" style="cursor:pointer"><input type="checkbox" data-setup-agent="${a.name}" ${a.checked ? "checked" : ""} /> ${a.name}${a.disabled ? "（已停用）" : ""}</label>`).join("")}
        </div>
        <div class="searchbar" id="setup-actions">
          <button class="btn" id="setup-plan">检测并预览</button>
          <button class="btn primary hidden" id="setup-run">确认执行接入</button>
        </div>
        <div id="setup-report"></div>
      </div>`)}

    ${panel("ai", `
      <h2>AI 整理</h2>
      <div class="memcard">
        <div class="content">连接兼容 OpenAI Chat Completions 的 API，为动态生成带来源的项目摘要。未配置时，动态、搜索和归档仍可使用。</div>
        <div class="ai-settings-note">单次仅发送对话标题、末条 Agent 回复、任务、产物路径和交接摘要，不发送完整对话、文件内容或 API Key。模型输出先作为建议显示，点击保存后才写入项目记忆。</div>
        ${aiSettingsError ? `<div class="digest digest-error">系统凭据库不可用：${esc(aiSettingsError)}</div>` : ""}
        <div class="form-grid ai-settings-form">
          <label>API 根地址<input id="ai-base-url" type="url" value="${esc(ai.base_url)}" placeholder="https://api.openai.com/v1" /></label>
          <label>模型名称<input id="ai-model" type="text" value="${esc(ai.model)}" placeholder="填写账号可用的模型 ID" /></label>
          <label>单次发送上限<input id="ai-max-input" type="number" min="4000" max="200000" step="1000" value="${esc(ai.max_input_chars)}" /></label>
          <label>API Key<input id="ai-api-key" type="password" autocomplete="off" placeholder="${ai.key_configured ? "已保存；留空保持不变" : "保存在系统凭据库"}" /></label>
        </div>
        <div class="searchbar">
          <button class="btn primary" id="ai-settings-save">保存设置</button>
          <button class="btn" id="ai-key-clear" ${ai.key_configured && ai.key_source !== "environment" ? "" : "disabled"}>移除 API Key</button>
          <span id="ai-settings-state" class="setting-state">${ai.key_configured ? `API Key：${ai.key_source === "environment" ? "由环境变量提供" : "已保存在系统凭据库"}` : "尚未保存 API Key"}</span>
        </div>
      </div>`)}

    ${panel("general", `
      <h2>后台运行</h2>
      <div class="memcard"><div class="meta" style="margin-top:0">关闭窗口后保留在系统托盘，每分钟采集已启用的数据源。点击托盘图标可打开窗口；右键选择「退出」可停止后台运行。</div></div>
      <h2>回收站自动清理</h2>
      <div class="memcard">
        <div class="meta" style="margin-top:0">开启后每次启动自动彻底删除超过 ${30} 天的回收站对话（对象先归档备份，非直接销毁）。默认关闭——物理删除不可逆。</div>
        <div class="searchbar">
          <label style="display:flex;align-items:center;gap:8px;cursor:pointer">
            <input type="checkbox" id="auto-purge" ${autoPurge ? "checked" : ""} />
            <span>超过 ${30} 天自动彻底删除（启动时执行）</span>
          </label>
        </div>
      </div>

      <h2>关于</h2>
      <div class="memcard"><div class="meta" style="margin-top:0">
        版本 ${esc(info.app_version)} · schema v${esc(info.schema_version)} · 数据目录 ${esc(home)}
      </div>
      <div class="searchbar" style="margin-top:8px">
        <button class="btn" id="update-check">检查更新</button>
        <span id="update-report" style="color:var(--faint);font-size:12px"></span>
      </div>
      <div class="searchbar" style="margin-top:8px">
        <button class="btn" id="doctor-run">运行自检（doctor）</button>
        <span style="color:var(--faint);font-size:12px">schema / FTS / vault 抽验 / 对象缺失 / 快照新鲜度——只读，不改数据</span>
      </div>
      <div id="doctor-report"></div>
      <div class="content" style="margin-top:8px">近 7 天使用（本地统计，不外传）：</div>
      <table>${(info.usage_last_7d || []).map((u) => `<tr><td>${esc(u.source)}</td><td>${esc(u.name)}</td><td class="c-num">${u.count}</td></tr>`).join("") || '<tr><td class="empty">暂无记录</td></tr>'}</table>
      </div>`)}`;

  // tab 切换只做显隐，不重新拉数据（四个面板一次渲染全量）
  document.querySelectorAll("#page-settings .subtab").forEach((b) => {
    b.onclick = () => {
      settingsTab = b.dataset.stab;
      document.querySelectorAll("#page-settings .subtab").forEach((x) => x.classList.toggle("on", x === b));
      document.querySelectorAll("#page-settings .subpanel").forEach((p) => p.classList.toggle("on", p.dataset.spanel === settingsTab));
      snapScrollboxTables($("#page-settings")); // 刚显形的面板渲染时不可见，补吸附
    };
  });

  const aiSettingsArgs = (clearKey = false) => ({
    baseUrl: $("#ai-base-url").value.trim(),
    model: $("#ai-model").value.trim(),
    maxInputChars: Number($("#ai-max-input").value),
    apiKey: $("#ai-api-key").value.trim() || null,
    clearKey,
  });
  $("#ai-settings-save").onclick = async () => {
    const btn = $("#ai-settings-save"), state = $("#ai-settings-state");
    btn.disabled = true; state.textContent = "正在保存…";
    try {
      const next = await invoke("ai_settings_save", aiSettingsArgs(false));
      $("#ai-api-key").value = "";
      state.textContent = next.key_configured ? "设置已保存，API Key 未显示" : "设置已保存，尚未保存 API Key";
      toast("AI 整理设置已保存");
    } catch (e) { state.textContent = `保存失败：${String(e)}`; }
    finally { btn.disabled = false; }
  };
  $("#ai-key-clear").onclick = async () => {
    const btn = $("#ai-key-clear"), state = $("#ai-settings-state");
    if (btn.disabled) return;
    btn.disabled = true; state.textContent = "正在移除…";
    try {
      await invoke("ai_settings_save", aiSettingsArgs(true));
      state.textContent = "API Key 已移除";
      toast("API Key 已移除");
    } catch (e) { state.textContent = `移除失败：${String(e)}`; btn.disabled = false; }
  };

  $("#agent-add-btn").onclick = async () => {
    const path = $("#agent-add-path").value.trim();
    if (!path) return;
    try {
      await invoke("agent_add_root", { agent: $("#agent-add-name").value, path });
      toast("已添加，下次 import 生效");
      renderSettings();
    } catch (e) { toast(String(e)); }
  };
  bindFolderPicker("#agent-add-pick", "#agent-add-path", "#agent-add-btn");
  document.querySelectorAll("#page-settings [data-agent-toggle]").forEach((box) => {
    box.onchange = async () => {
      try {
        await invoke("agent_set_enabled", { agent: box.dataset.agentToggle, enabled: box.checked });
        toast(box.checked ? "已启用：下次采集恢复该源" : "已停用：不再采集该源（历史数据保留）");
        renderSettings();
      } catch (e) { toast(String(e)); renderSettings(); }
    };
  });
  const td = $("#toggle-disabled");
  if (td) td.onclick = () => { showDisabledSources = !showDisabledSources; renderSettings(); };
  const tw = $("#toggle-watchlist");
  if (tw) tw.onclick = () => { showWatchlist = !showWatchlist; renderSettings(); };
  document.querySelectorAll("#page-settings [data-rmroot]").forEach((btn) => {
    btn.onclick = async () => {
      const [agent, ...rest] = btn.dataset.rmroot.split("|");
      try {
        await invoke("agent_remove_root", { agent, path: rest.join("|") });
        toast("已移除（已导入数据不受影响）");
        renderSettings();
      } catch (e) { toast(String(e)); }
    };
  });

  let bundleBusy = false, pendingMerge = null;
  const bundleButtons = ["#bundle-create", "#bundle-verify", "#bundle-restore", "#bundle-path", "#bundle-out", "#bundle-out-pick", "#bundle-path-pick"].map($);
  const setBundleBusy = (busy) => {
    bundleBusy = busy;
    bundleButtons.forEach(el => { el.disabled = busy; });
  };
  const clearMerge = () => {
    pendingMerge = null;
    $("#bundle-restore").textContent = "合并恢复";
  };
  $("#bundle-path").oninput = clearMerge;
  for (const [button, target, save] of [["#bundle-out-pick", "#bundle-out", true], ["#bundle-path-pick", "#bundle-path", false]]) {
    $(button).onclick = async () => {
      if (bundleBusy) return;
      const input = $(target), rep = $("#bundle-report");
      setBundleBusy(true);
      try {
        const path = await invoke("bundle_path_pick", { path: input.value.trim(), save });
        if (path !== null && input === $(target)) {
          input.value = path;
          clearMerge();
          rep.textContent = save ? "已选择保存位置，点击“创建备份”后写入" : "已选择备份，可校验或合并恢复";
        }
      } catch (e) { rep.textContent = `选择失败：${String(e)}`; }
      finally { setBundleBusy(false); }
    };
  }
  const bundleFailure = (r) => r.reason || (r.missing_referenced_objects?.length
    ? `缺少 ${r.missing_referenced_objects.length} 个引用对象`
    : r.database_errors?.join("；") || "对象、数据库或版本检查未通过");
  $("#bundle-create").onclick = async () => {
    if (bundleBusy) return;
    clearMerge(); setBundleBusy(true);
    const rep = $("#bundle-report"); rep.textContent = "正在创建完整备份…";
    try {
      const m = await invoke("bundle_create", { out: $("#bundle-out").value.trim() });
      rep.textContent = `已创建完整备份：${m.objects} 个对象，${(m.objects_bytes / 1024).toFixed(0)} KB`;
    } catch (e) { rep.textContent = `创建失败：${String(e)}`; }
    finally { setBundleBusy(false); }
  };
  $("#bundle-verify").onclick = async () => {
    const path = $("#bundle-path").value.trim();
    if (!path || bundleBusy) return;
    clearMerge(); setBundleBusy(true);
    const rep = $("#bundle-report"); rep.textContent = "正在校验备份…";
    try {
      const r = await invoke("bundle_verify", { path });
      rep.textContent = r.ok ? `校验通过：${r.objects} 个对象，数据库与引用完整，版本一致`
        : `校验失败：${bundleFailure(r)}`;
    } catch (e) { rep.textContent = `校验失败：${String(e)}`; }
    finally { setBundleBusy(false); }
  };
  $("#bundle-restore").onclick = async () => {
    const path = $("#bundle-path").value.trim();
    if (!path || bundleBusy) return;
    setBundleBusy(true);
    const rep = $("#bundle-report"); rep.textContent = "正在校验并计算合并结果…";
    try {
      const v = await invoke("bundle_verify", { path });
      if (!v.ok) { clearMerge(); rep.textContent = `校验失败，已阻止恢复：${bundleFailure(v)}`; return; }
      const plan = v.merge_plan;
      const signature = JSON.stringify([path, v.manifest, plan]);
      if (pendingMerge !== signature) {
        pendingMerge = signature;
        rep.textContent = `目标：${plan.home}。预计新增 ${plan.sessions_added} 个、替换 ${plan.sessions_replaced} 个、跳过 ${plan.sessions_skipped} 个对话。同 ID 以消息更多的一侧为准；同 ID 项目记忆保留当前库版本。`;
        $("#bundle-restore").textContent = "确认合并";
        rep.scrollIntoView({ block: "center" });
        return;
      }
      clearMerge(); rep.textContent = "正在恢复对象并合并数据库…";
      const r = await invoke("bundle_restore", { path, merge: true });
      rep.textContent = `合并完成：新增 ${r.merged.sessions_added} 个、替换 ${r.merged.sessions_replaced} 个、跳过 ${r.merged.sessions_skipped} 个对话。`;
    } catch (e) {
      clearMerge(); rep.textContent = `恢复失败：${String(e)}`;
    } finally { setBundleBusy(false); }
  };

  $("#auto-purge").onchange = async (e) => {
    await invoke("auto_purge_set", { enabled: e.target.checked });
    toast(e.target.checked
      ? "已开启：下次启动 app 时自动清理超期回收站"
      : "已关闭自动清理");
  };
  $("#update-check").onclick = async () => {
    const rep = $("#update-report"), btn = $("#update-check");
    btn.disabled = true;
    rep.textContent = "检查中…";
    try {
      const r = await invoke("update_check");
      if (!r.update_available) {
        rep.textContent = "已是最新版本";
      } else {
        rep.innerHTML = `新版本 v${esc(r.latest)} 可用 <button class="btn small primary" id="update-install">下载并安装</button><button class="btn small" id="update-goto">查看版本</button>`;
        $("#update-goto").onclick = () => invoke("open_url", { url: r.url }).catch((e) => toast(String(e)));
        $("#update-install").onclick = async () => {
          if (!confirm(`将下载并安装 yourmem v${r.latest}，完成后软件会自动重启。继续？`)) return;
          const install = $("#update-install"), link = $("#update-goto");
          install.disabled = true; link.disabled = true; btn.disabled = true;
          install.textContent = "准备下载…";
          let unlisten = null;
          try {
            if (window.__TAURI__.event?.listen) {
              unlisten = await window.__TAURI__.event.listen("update-progress", ({ payload }) => {
                if (payload?.phase === "installing") {
                  install.textContent = "正在安装…";
                } else if (payload?.phase === "downloading") {
                  const done = Number(payload.downloaded || 0), total = Number(payload.total || 0);
                  install.textContent = total > 0 ? `下载中 ${Math.min(100, Math.round(done / total * 100))}%` : "正在下载…";
                }
              });
            }
            await invoke("update_install");
          } catch (e) {
            rep.innerHTML = `更新失败：${esc(String(e))} <button class="btn small" id="update-goto">前往 GitHub 下载</button>`;
            $("#update-goto").onclick = () => invoke("open_url", { url: r.url }).catch((err) => toast(String(err)));
            btn.disabled = false;
          } finally {
            if (unlisten) unlisten();
          }
        };
      }
    } catch (e) {
      rep.textContent = `检查失败：${String(e)}`;
    } finally {
      btn.disabled = false;
    }
  };
  $("#doctor-run").onclick = async () => {
    $("#doctor-report").innerHTML = '<div class="meta">自检中…</div>';
    try {
      const r = await invoke("doctor");
      const icon = (s) => s === "ok" ? '<span class="cap-yes">✓</span>' : s === "warn" ? '<span class="cap-partial">⚠</span>' : '<span class="proof-bad">✗</span>';
      $("#doctor-report").innerHTML = `
        <div class="meta" style="margin-top:6px">${r.ok ? '<span class="cap-yes">✓ 全部通过</span>' : '<span class="proof-bad">✗ 有失败项</span>'} · ${fmtTime(r.checked_at)}</div>
        <table>${r.checks.map((c) => `<tr><td>${icon(c.status)}</td><td style="white-space:nowrap">${esc(c.name)}</td><td style="color:var(--dim)">${esc(c.detail)}</td></tr>`).join("")}</table>`;
    } catch (e) { $("#doctor-report").innerHTML = `<div class="meta">✗ 自检失败：${esc(e)}</div>`; }
  };
  let plannedSetupAgents = [];
  const selectedSetupAgents = () => Array.from(document.querySelectorAll("#page-settings [data-setup-agent]"))
    .filter((box) => box.checked).map((box) => box.dataset.setupAgent);
  document.querySelectorAll("#page-settings [data-setup-agent]").forEach((box) => {
    box.onchange = () => {
      plannedSetupAgents = [];
      $("#setup-run").classList.add("hidden");
      $("#setup-report").innerHTML = '';
    };
  });
  $("#setup-plan").onclick = async () => {
    const btn = $("#setup-plan");
    const agents = selectedSetupAgents();
    if (!agents.length) {
      $("#setup-report").innerHTML = '<div class="meta proof-bad">请至少选择一个要接入的 agent。</div>';
      return;
    }
    btn.disabled = true;
    $("#setup-report").innerHTML = '<div class="meta">检测中…</div>';
    try {
      const p = await invoke("setup_plan", { agents });
      $("#setup-report").innerHTML = (p.agents || []).map((a) => `
        <div class="meta">${esc(a.agent)}：${a.status === "skip" ? "未检测到，跳过" :
          (a.actions || []).map((x) => `${x.kind === "register_mcp" ? "注册 MCP" : "全局指令"} → ${esc(x.path)}（${x.status === "done" ? "已配置" : x.status === "stale" ? "旧版待更新" : "待写入"}）`).join("；")}</div>`).join("");
      plannedSetupAgents = agents;
      $("#setup-run").classList.remove("hidden");
    } catch (e) {
      $("#setup-report").innerHTML = `<div class="meta proof-bad">✗ 检测失败：${esc(String(e))}</div>`;
    } finally {
      btn.disabled = false;
    }
  };
  $("#setup-run").onclick = async () => {
    const btn = $("#setup-run");
    if (!plannedSetupAgents.length) return;
    btn.disabled = true;
    $("#setup-report").innerHTML = '<div class="meta">执行中…</div>';
    try {
      const r = await invoke("setup_run", { agents: plannedSetupAgents });
      $("#setup-report").innerHTML = `<div class="meta">✓ 完成。验证方式：${esc(r.verify)}</div>`;
      $("#setup-run").classList.add("hidden");
    } catch (e) {
      $("#setup-report").innerHTML = `<div class="meta proof-bad">✗ 接入失败：${esc(String(e))}</div>`;
    } finally {
      btn.disabled = false;
    }
  };
  // 彻底删除的离线档案：查看占用 / 单删 / 清空（armed 二次确认）
  const fmtBytes = (n) => n >= 1073741824 ? `${(n / 1073741824).toFixed(2)} GB`
    : n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : `${(n / 1024).toFixed(1)} KB`;
  const drawArchives = async () => {
    let a;
    try { a = await invoke("purge_archives"); } catch (e) {
      $("#archives-report").innerHTML = `<div class="meta">✗ 读取失败：${esc(String(e))}</div>`;
      return;
    }
    $("#archives-clear").classList.toggle("hidden", a.archives.length === 0);
    $("#archives-report").innerHTML = a.archives.length
      ? `<div class="meta">共 ${a.archives.length} 项 · ${fmtBytes(a.total_bytes)}</div><table>${a.archives.map((x) => `
          <tr><td style="white-space:nowrap">${esc(x.name)}</td><td>${fmtBytes(x.bytes)}</td><td>${fmtTime(x.modified)}</td>
          <td class="c-act"><button class="btn small danger" data-archive-del="${esc(x.name)}">删除</button></td></tr>`).join("")}</table>`
      : '<div class="meta">暂无档案</div>';
    document.querySelectorAll("[data-archive-del]").forEach((btn) => {
      armButton(btn, "删除", "确认删除该档案？不可恢复", async () => {
        await invoke("purge_archive_delete", { name: btn.dataset.archiveDel });
        toast("档案已移除");
        drawArchives();
      });
    });
  };
  $("#archives-list").onclick = () => { $("#archives-report").innerHTML = '<div class="meta">统计中…</div>'; drawArchives(); };
  armButton($("#archives-clear"), "清空全部", "确认清空全部档案？不可恢复", async () => {
    const r = await invoke("purge_archive_clear");
    toast(`已清空 ${r.removed} 项档案`);
    drawArchives();
  });
  // 存储占用 + 搜索索引范围（1.0.1 轻量化）
  const drawStorage = async () => {
    try {
      const u = await invoke("storage_usage");
      const st = await invoke("index_status");
      $("#storage-report").innerHTML = `
        <div class="meta" style="margin:12px 14px 2px"><strong>核心数据</strong> · 软件运行和恢复原文需要<br><span style="overflow-wrap:anywhere">${esc(u.data_dir)}</span></div>
        <table>
        <tr><td>数据库与搜索索引</td><td class="c-num">${fmtBytes(u.db_bytes)}</td></tr>
        <tr><td>会话原文归档</td><td class="c-num">${fmtBytes(u.objects_bytes)}</td></tr>
        ${st.free_bytes > 1048576 ? `<tr><td>其中空闲页可回收</td><td class="c-num">${fmtBytes(st.free_bytes)}</td></tr>` : ""}
        </table>
        <div class="meta" style="margin:14px 14px 2px"><strong>备份与导出</strong> · 手动生成，不影响软件日常使用<br><span style="overflow-wrap:anywhere">${esc(u.backups_dir)}</span></div>
        <table><tr><td>快照、导出与删除档案</td><td class="c-num">${fmtBytes(u.backups_bytes)}</td></tr></table>`;
    } catch (e) {
      $("#storage-report").innerHTML = `<div class="meta">✗ 读取失败：${esc(String(e))}</div>`;
    }
  };
  $("#storage-usage").onclick = () => { $("#storage-report").innerHTML = '<div class="meta">统计中…</div>'; drawStorage(); };
  let curBackupDir = bd.effective;
  $("#backup-dir-save").onclick = async () => {
    const rep = $("#backup-dir-report"), btn = $("#backup-dir-save");
    btn.disabled = true;
    try {
      const r = await invoke("backup_dir_set", { path: $("#backup-dir").value.trim() });
      const output = $("#bundle-out");
      const oldPrefix = curBackupDir.replaceAll("\\", "/").replace(/\/$/, "") + "/";
      const oldOutput = output.value.replaceAll("\\", "/");
      if (oldOutput.startsWith(oldPrefix)) output.value = r.effective.replace(/[\\/]$/, "") + "/" + oldOutput.slice(oldPrefix.length);
      curBackupDir = r.effective;
      $("#snapshot-refresh").click();
      const moved = Number(r.moved_files || 0);
      rep.textContent = `当前：${r.effective}${moved ? `（已迁移 ${moved} 个文件）` : ""}${r.cleanup_warning ? `；${r.cleanup_warning}` : ""}`;
      toast(moved ? `备份位置已保存，已迁移 ${moved} 个文件` : "备份位置已保存");
    } catch (e) {
      rep.textContent = `✗ ${String(e)}`;
    } finally {
      btn.disabled = false;
    }
  };
  $("#backup-dir-pick").onclick = async () => {
    const btn = $("#backup-dir-pick"), input = $("#backup-dir"), save = $("#backup-dir-save");
    if (save.disabled) return;
    btn.disabled = true;
    save.disabled = true;
    try {
      const path = await invoke("backup_dir_pick", { path: input.value.trim() || curBackupDir });
      if (path !== null && input === $("#backup-dir")) {
        input.value = path;
        $("#backup-dir-report").textContent = `已选择：${path}，点击“保存”后生效`;
      }
    } catch (e) { toast(String(e)); }
    finally { btn.disabled = false; save.disabled = false; }
  };
  $("#storage-compact").onclick = async () => {
    const btn = $("#storage-compact");
    btn.disabled = true;
    $("#storage-report").innerHTML = '<div class="meta">整理中，需数分钟…</div>';
    try {
      await invoke("compact_db");
      toast("整理完成");
      await drawStorage();
    } catch (e) {
      $("#storage-report").innerHTML = `<div class="meta">✗ 整理失败：${esc(String(e))}</div>`;
    } finally {
      btn.disabled = false;
    }
  };
  $("#tools-index").onchange = async (ev) => {
    const full = ev.target.checked;
    const box = ev.target, state = $("#tools-index-state");
    box.disabled = true;
    state.textContent = full ? "重建索引中，需数分钟…" : "回收空间中，需数分钟…";
    try {
      const r = await invoke("index_set_tools", { full });
      toast(full ? "工具输出已可全文搜索" : "已切回轻量索引");
      state.textContent = `已索引 ${r.indexed_tool_rows} / ${r.tool_messages} 行`;
    } catch (e) {
      state.textContent = "";
      box.checked = !full;
      toast(`操作失败：${String(e)}`);
    } finally {
      box.disabled = false;
    }
  };
  bindSnapshotPanel({ invoke, root: $("#page-settings"), bundleInput: $("#bundle-path"), esc });
  snapScrollboxTables($("#page-settings"));
}

// ---------------------------------------------------------------- search
async function renderSearch() {  $("#page-search").innerHTML = `
    <h1>搜索 <span class="en">Search</span></h1>
    <div class="searchbar">
      <input type="text" id="q" placeholder="搜所有 agent 的对话（中英文均可）" />
      <select id="q-agent">
        ${AGENT_LIST.map((agent) => `<option value="${esc(agent)}">${esc(agent || "全部 agent")}</option>`).join("")}
      </select>
      <button class="btn primary" id="q-go">搜索</button>
    </div>
    <div id="q-scope" class="meta">正在读取搜索范围…</div>
    <div class="meta">每条消息最多检索前 20 万字符，完整原文可从对话详情导出。</div>
    <div id="q-results" aria-live="polite"></div>`;
  const scope = $("#q-scope");
  invoke("index_status").then(r => {
    if (scope !== $("#q-scope")) return;
    scope.textContent = r.tool_index_full ? "搜索范围：对话及工具输出" : "搜索范围：对话；工具输出仅支持少于 3 字的短词，全文索引可在设置中开启";
  }).catch(() => { if (scope === $("#q-scope")) scope.textContent = "搜索范围读取失败，可在设置中查看索引状态"; });
  const results = $("#q-results");
  let request = 0;
  const go = async () => {
    const q = $("#q").value.trim();
    if (!q) return;
    const current = ++request;
    let d;
    try {
      d = await invoke("search", { query: q, agent: $("#q-agent").value || null, limit: 50 });
    } catch (e) {
      if (current === request && results === $("#q-results")) toast(String(e));
      return;
    }
    if (current !== request || results !== $("#q-results")) return;
    const resultCards = d.results.map((r) => `
      <div class="hit" data-sid="${esc(r.session_id)}" data-line="${r.line_no}" data-message="${r.message_id}">
        <div class="snippet">${esc(r.snippet)}</div>
        <div class="meta"><span class="pill ${esc(r.agent)}">${esc(r.agent)}</span>
          <span class="pill">${esc(r.kind)}</span> ${esc(r.project || "—")} · ${fmtTime(r.timestamp)}</div>
      </div>`).join("");
    $("#q-results").innerHTML = `<div class="search-results-head"><span>搜索结果</span><span class="search-results-count">${d.results.length} 条</span></div>
      <div class="scrollbox search-results">${resultCards || '<div class="empty">暂无结果</div>'}
        ${resultCards ? `<div class="search-results-tail">已展示全部 ${d.results.length} 条 · 点击结果查看完整对话</div>` : ""}
      </div>`;
    document.querySelectorAll("#q-results .hit").forEach((el) => {
      el.onclick = () => showSession(el.dataset.sid, Number(el.dataset.line), false, null, Number(el.dataset.message));
    });
  };
  $("#q-go").onclick = go;
  $("#q").onkeydown = (e) => { if (e.key === "Enter") go(); };
  $("#q").focus();
}

// ---------------------------------------------------------------- nav
// 三态路由：每页统一 加载中 → 内容 / 错误卡（带重试）。空态由各页自己给。
const stateCard = (kind, msg) => `<div class="state ${kind}">${msg}</div>`;
const loadCard = () => stateCard("loading", "加载中…");
const errCard = (e) => stateCard("error", `加载失败：${esc(String(e))} — <button class="btn small" data-retry>重试</button>`);
async function route(name, fn) {
  const el = $("#page-" + name);
  el.innerHTML = loadCard();
  try {
    await fn();
  } catch (e) {
    el.innerHTML = errCard(e);
    const b = el.querySelector("[data-retry]");
    if (b) b.onclick = () => route(name, fn);
  }
}
const pageRenderers = { today: renderToday, activity: renderActivity, projects: renderProjects, sessions: renderSessions,
  memory: renderMemory, search: renderSearch, settings: renderSettings };
const pages = Object.fromEntries(Object.entries(pageRenderers).map(([name, fn]) => [name, () => route(name, fn)]));
document.querySelectorAll(".nav").forEach((btn) => {
  btn.onclick = () => {
    document.querySelectorAll(".nav").forEach((b) => b.classList.remove("active"));
    document.querySelectorAll(".page").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    $(`#page-${btn.dataset.page}`).classList.add("active");
    pages[btn.dataset.page]();
  };
});

$("#btn-import").onclick = async () => {
  const btn = $("#btn-import"), label = $("#btn-import span");
  if (btn.disabled) return;
  btn.disabled = true;
  btn.classList.add("busy");
  label.textContent = "采集中…";
  try {
    const r = await invoke("import_now");
    toast(`完成：新增 ${r.messages_added} 条消息 / ${r.lines_archived} 行归档`);
    const current = document.querySelector(".nav.active").dataset.page;
    try { await pageRenderers[current](); }
    catch (e) { toast(`采集完成，页面更新失败：${e}`); }
  } catch (e) {
    toast(`采集失败：${e}`);
  } finally {
    btn.disabled = false;
    btn.classList.remove("busy");
    label.textContent = "采集新对话";
  }
};

(async () => {
  // 首次启动向导：须在首屏渲染前探测（页面渲染即建库，之后判定恒为 false）
  let fr = null;
  try { fr = await invoke("first_run_state"); } catch (e) {}
  pages.today();
  if (fr && fr.first_run) showWizard(fr);
})();

// ---------------------------------------------------------------- first-run wizard
// 安装使用向导（1.0.2）：欢迎 + 手动选择备份位置，一步完成，可跳过用默认。
// 只对新装出现（is_first_run：数据目录无 config.json 也无 yourmem.db）。
function showWizard(fr) {
  if (document.getElementById("wizard-overlay")) return;
  const ov = document.createElement("div");
  ov.id = "wizard-overlay";
  ov.innerHTML = `
    <div class="wiz-card">
      <h2 style="margin-top:0">欢迎使用 yourmem</h2>
      <p class="meta">把你与 AI 编程助手的对话自动归档、统一检索，数据保存在你自己的电脑上。</p>
      <h3>备份与导出存到哪里？</h3>
      <p class="meta">这里存放手动生成的数据库快照、会话导出和删除档案。核心数据仍保存在 ${esc(fr.home)}；留空使用默认位置。</p>
      <div class="searchbar">
        <input type="text" id="wiz-backup-dir" style="flex:1" placeholder="绝对路径，如 D:\\yourmem-backup" value="${esc(fr.suggested || "")}" />
        <button class="btn" id="wiz-pick">选择文件夹</button>
        <button class="btn" id="wiz-default">用默认</button>
      </div>
      <div class="meta" id="wiz-hint" style="margin:6px 0 12px">默认位置：<span id="wiz-default-path"></span></div>
      <div class="searchbar" style="justify-content:flex-end">
        <button class="btn" id="wiz-skip">跳过</button>
        <button class="btn primary" id="wiz-done">保存并开始使用</button>
      </div>
    </div>`;
  document.body.appendChild(ov);
  invoke("backup_dir_get").then((bd) => {
    if (ov.isConnected) $("#wiz-default-path").textContent = bd.effective;
  }).catch(() => {});
  const close = () => ov.remove();
  $("#wiz-pick").onclick = async () => {
    const input = $("#wiz-backup-dir"), hint = $("#wiz-hint");
    const buttons = ["#wiz-pick", "#wiz-done", "#wiz-skip", "#wiz-default"].map($);
    buttons.forEach(b => { b.disabled = true; });
    try {
      const path = await invoke("backup_dir_pick", { path: input.value.trim() || fr.backup_dir || "" });
      if (path !== null && ov.isConnected) {
        input.value = path;
        hint.textContent = "已选择文件夹，保存后生效";
      }
    } catch (e) { if (ov.isConnected) hint.textContent = `选择失败：${String(e)}`; }
    finally { buttons.forEach(b => { b.disabled = false; }); }
  };
  $("#wiz-default").onclick = () => { $("#wiz-backup-dir").value = ""; };
  $("#wiz-backup-dir").onkeydown = (e) => { if (e.key === "Enter") $("#wiz-done").click(); };
  $("#wiz-skip").onclick = () => { close(); toast("使用默认备份位置，之后可在 设置 → 备份 修改"); };
  $("#wiz-done").onclick = async () => {
    const btn = $("#wiz-done");
    btn.disabled = true;
    try {
      const r = await invoke("backup_dir_set", { path: $("#wiz-backup-dir").value.trim() });
      close();
      toast(`备份位置：${r.effective}`);
    } catch (e) {
      $("#wiz-hint").textContent = `✗ ${String(e)}`;
      btn.disabled = false;
    }
  };
  $("#wiz-backup-dir").focus();
}
invoke("update_check").then((r) => {
  if (r && r.update_available) toast(`新版本 v${r.latest} 可用（设置 → 通用 → 检查更新）`);
}).catch(() => {});


// ---------------------------------------------------------------- memory graph
// 演变链图（2026-09-01 改版，用户反馈"按项目分组的卡片+同列弧线看不懂，
// 要时间轴/树形"）：复用谱系图的确定性分层布局——列 = 演变深度（被取代的
// 旧版本在左，链头最新版在右），列内按 created_at 排序；无取代关系的孤立
// 记忆自然落在第 0 列，成为时间轴。竞品扫描（COMPETITORS.md 2026-09-01）：
// Zep 有时序语义无 UI、cognee/mem0/supermemory 有力导向快照图无演变——
// 链式 DAG 的确定性分层是竞品空白形态。
function memoryGraphHtml(mems) {
  if (!mems.length) return '<div class="empty">暂无记忆</div>';
  const byId = new Map(mems.map((m) => [m.id, m]));
  const graphEdges = mems.filter(m => m.superseded_by && byId.has(m.superseded_by)).map(m => [m.id, m.superseded_by]);
  const edgeCount = graphEdges.length;
  const missingEdges = mems.filter(m => m.superseded_by && !byId.has(m.superseded_by)).length;
  const { depth, unresolved } = graphDepths(mems.map(m => m.id), graphEdges);
  const depthOf = id => depth.get(id);
  const cols = new Map();
  for (const m of mems) {
    const dd = depthOf(m.id);
    if (!cols.has(dd)) cols.set(dd, []);
    cols.get(dd).push(m);
  }
  cols.forEach((list) => list.sort((a, b) => (a.created_at || "").localeCompare(b.created_at || "")));
  const CW = 220, RH = 84, NW = 196, NH = 66;
  const pos = new Map();
  for (const [dd, list] of cols) list.forEach((m, i) => pos.set(m.id, { x: dd * CW, y: i * RH }));
  const W = (Math.max(...cols.keys()) + 1) * CW;
  const H = Math.max(...[...cols.values()].map((l) => l.length)) * RH;

  const nodeHtml = mems.map((m) => {
    const { x, y } = pos.get(m.id);
    return `<div class="gnode mnode-g" data-mid="${esc(m.id)}" data-sup="${esc(m.superseded_by || "")}" data-src="${esc(m.source_session_id || "")}" style="left:${x}px;top:${y}px" title="${esc(m.content || "")}">
      <span class="pill ${esc(m.status)}">${esc(m.status === "superseded" ? "被取代" : m.status)}</span>
      <span class="gnode-title">${esc((m.content || "").slice(0, 40))}</span>
      <span class="gnode-sub">${esc(m.type)} · ${esc(m.project || "全局")} · ${fmtTime(m.created_at)}${m.source_session_id ? ' · <span class="src" title="跳到来源对话">来源</span>' : ""}</span>
    </div>`;
  }).join("");

  const edgeSvg = mems.filter((m) => m.superseded_by && pos.has(m.superseded_by)).map((m) => {
    const a = pos.get(m.id), b = pos.get(m.superseded_by);
    const x1 = a.x + NW, y1 = a.y + NH / 2, x2 = b.x, y2 = b.y + NH / 2;
    const mx = (x1 + x2) / 2;
    return `<path d="M ${x1} ${y1} C ${mx} ${y1}, ${mx} ${y2}, ${x2} ${y2}" marker-end="url(#lg-arrow)"/>`;
  }).join("");

  return `${missingEdges ? `<div class="meta">${missingEdges} 条关系的目标不可用</div>` : ""}${unresolved.length ? `<div class="meta">部分历史关系包含环路，未按演变顺序排列</div>` : ""}<h2>关系图（${mems.length} 节点 / ${edgeCount} 条取代链） <button class="btn small graph-zoom" data-title="关系图 · 滚轮/触控板滚动查看">放大</button></h2>
    <div class="scrollbox lgraph-scroll"><div class="lgraph" style="width:${W}px;height:${H}px">
      <svg class="lgraph-edges" width="${W}" height="${H}">
        <defs><marker id="lg-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="var(--accent)"/></marker></defs>
        ${edgeSvg}
      </svg>
      ${nodeHtml}
    </div></div>`;
}

// 节点点击：高亮整条演变链（沿 superseded_by 向两侧走）；来源指针单独点。
// 边已随 HTML 静态绘出，不再有 drawEdges 覆盖层。
function bindMemoryGraph(mems) {
  const container = document.querySelector("#page-memory .lgraph");
  if (!container) return;
  container.querySelectorAll(".mnode-g").forEach((node) => {
    node.onclick = (e) => {
      if (e.target.classList.contains("src")) {
        const sid = node.dataset.src;
        if (sid) { const m = mems.find(m => m.id === node.dataset.mid); showSession(sid, m?.source_line_no ?? null); }
        return;
      }
      container.querySelectorAll(".mnode-g.hl").forEach((n) => n.classList.remove("hl"));
      const neighbors = new Map(mems.map(m => [m.id, []]));
      for (const m of mems) if (neighbors.has(m.superseded_by)) {
        neighbors.get(m.id).push(m.superseded_by);
        neighbors.get(m.superseded_by).push(m.id);
      }
      const chain = new Set([node.dataset.mid]), queue = [node.dataset.mid];
      for (let i = 0; i < queue.length; i++) for (const id of neighbors.get(queue[i]) || []) {
        if (!chain.has(id)) { chain.add(id); queue.push(id); }
      }
      container.querySelectorAll(".mnode-g").forEach(n => { if (chain.has(n.dataset.mid)) n.classList.add("hl"); });
    };
  });
}


// ---------------------------------------------------------------- 键盘优先（UI-DESIGN §0，0.4.1）
// `/` 跳搜索 · j/k 行间移动 · Enter 打开 · Delete 删除（二次按键确认，进回收站）
// · Esc 关 drawer · Cmd+C 复制 resume（drawer 打开且无文本选区时）。
// 输入控件聚焦时一律放行（Esc 除外——关 drawer 比 input 失焦更符合直觉）。
let kbSel = null;        // 当前键盘选中的 tr[data-sid]
let kbDelArmedAt = 0;    // Delete 二次确认计时
let kbDelTarget = "";

document.addEventListener("keydown", (e) => {
  const tag = (e.target.tagName || "").toLowerCase();
  const typing = tag === "input" || tag === "textarea" || tag === "select" || e.target.isContentEditable;
  if (e.key === "Escape") {
    // 谱系图全屏覆盖层打开时 Esc 只关覆盖层（覆盖层自己的捕获 handler 主责，
    // 这里是兜底：捕获被跳过/重复投递时也不误关 drawer）
    if (document.getElementById("graph-overlay")) return;
    if (document.getElementById("activity-ai-overlay")) { closeActivityAi(); return; }
    closeDrawer();
    return;
  }
  if (typing) return;

  if (e.key === "/" && !e.metaKey && !e.ctrlKey && !e.altKey) {
    e.preventDefault();
    document.querySelector('.nav[data-page="search"]').click(); // renderSearch 自带 focus
    return;
  }
  if ((e.metaKey || e.ctrlKey) && e.key === "c") {
    const drawerOpen = !$("#drawer").classList.contains("hidden");
    if (drawerOpen && drawer.resume && window.getSelection().isCollapsed) {
      e.preventDefault();
      navigator.clipboard.writeText(drawer.resume);
      toast("已复制 resume 命令");
    }
    return; // 有选区时走系统默认复制
  }
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  if (!["j", "k", "Enter", "Delete", "Backspace"].includes(e.key)) return;

  const page = document.querySelector(".page.active");
  if (!page) return;
  const rows = [...page.querySelectorAll("tr[data-sid]")];
  if (!rows.length) return;

  if (e.key === "j" || e.key === "k") {
    e.preventDefault();
    let i = rows.indexOf(kbSel);
    if (i === -1) i = e.key === "j" ? -1 : rows.length; // 无选中时从端点进场
    const next = Math.min(Math.max(i + (e.key === "j" ? 1 : -1), 0), rows.length - 1);
    if (kbSel) kbSel.classList.remove("kb-sel");
    kbSel = rows[next];
    kbSel.classList.add("kb-sel");
    kbSel.scrollIntoView({ block: "nearest" });
    return;
  }
  if (!kbSel || !rows.includes(kbSel)) return;

  if (e.key === "Enter") {
    e.preventDefault();
    showSession(kbSel.dataset.sid);
    return;
  }
  // Delete 只在对话主列表生效（回收站里的彻底删除有保留期门控，不走键盘）。
  // 软删进回收站可恢复，但仍沿用二次确认纪律：4 秒内按第二次才执行。
  if ((e.key === "Delete" || e.key === "Backspace") && page.id === "page-sessions" && !sessTrash) {
    e.preventDefault();
    const sid = kbSel.dataset.sid;
    if (kbDelTarget === sid && Date.now() - kbDelArmedAt < 4000) {
      kbDelArmedAt = 0;
      invoke("session_delete", { sessionId: sid }).then(() => {
        toast("已删除（回收站可恢复）");
        kbSel = null;
        renderSessions();
      }).catch((err) => toast(String(err)));
    } else {
      kbDelArmedAt = Date.now();
      kbDelTarget = sid;
      toast("再按一次 Delete 确认删除（进回收站，可恢复）");
    }
  }
});


// ---------------------------------------------------------------- 谱系图（§7.4，0.4.2）
// nodes: [{session_id, agent, started_at, messages, title, tail, ext, deleted}]；edges: [{p, c, lt}]。
// 确定性布局：层 = 父链深度，层内按 started_at 排序——不做力导向，
// 同一份数据永远画出同一张图（与 mermaid 导出同纪律：只画链上节点）。
// 横排 = root 在左代际向右；竖排 = root 在上代际向下（2026-09-01 用户反馈竖排
// 更直观，默认竖排可切换，drawer/卷宗/全屏覆盖层三处共享 graphVertical）。
// 节点标题化（2026-08-31 用户反馈"要进度树不要 id 链"）：title=首条用户消息
// （这段对话要干什么，无 LLM 摘要口径），tail=末条 assistant（停在哪）进 tooltip。
let graphVertical = true;
let lastGraphArgs = null; // 最近一次渲染参数 {nodes, edges, currentSid}，换向/覆盖层重渲染用
function lineageGraphHtml(nodes, edges, currentSid = null) {
  if (!edges.length) return "";
  lastGraphArgs = { nodes, edges, currentSid };
  const byId = new Map(nodes.map((n) => [n.session_id, n]));
  const inGraph = new Map(); // sid -> 节点数据（跨视图父/已清除/回收站为 null 或带标记）
  for (const e of edges) {
    if (!inGraph.has(e.c)) inGraph.set(e.c, byId.get(e.c));
    if (!inGraph.has(e.p)) inGraph.set(e.p, byId.get(e.p) || null);
  }
  const { depth, unresolved } = graphDepths([...inGraph.keys()], edges.map(e => [e.p, e.c]));
  const depthOf = id => depth.get(id);
  const cols = new Map();
  for (const sid of inGraph.keys()) {
    const dd = depthOf(sid);
    if (!cols.has(dd)) cols.set(dd, []);
    cols.get(dd).push(sid);
  }
  cols.forEach((list) => list.sort((a, b) => {
    const ta = inGraph.get(a), tb = inGraph.get(b);
    return ((ta && ta.started_at) || a).localeCompare((tb && tb.started_at) || b);
  }));
  const CW = 220, RH = 84, NW = 196, NH = 66;
  const pos = new Map();
  for (const [dd, list] of cols) list.forEach((sid, i) =>
    pos.set(sid, graphVertical ? { x: i * CW, y: dd * RH } : { x: dd * CW, y: i * RH }));
  const maxDepth = Math.max(...cols.keys());
  const maxLen = Math.max(...[...cols.values()].map((l) => l.length));
  const W = graphVertical ? maxLen * CW : (maxDepth + 1) * CW;
  const H = graphVertical ? (maxDepth + 1) * RH : maxLen * RH;

  const nodeHtml = [...inGraph.entries()].map(([sid, t]) => {
    const { x, y } = pos.get(sid);
    const cur = sid === currentSid;
    if (!t || t.ext || t.deleted) {
      const why = !t || t.ext ? "跨项目父节点或已清除" : "回收站中";
      return `<div class="gnode ext${cur ? " cur" : ""}" style="left:${x}px;top:${y}px" title="${esc(sid)}（${why}）">${esc(shortId(sid))}</div>`;
    }
    return `<div class="gnode${cur ? " cur" : ""}" data-sid="${esc(sid)}" style="left:${x}px;top:${y}px" title="${esc(sid)}${t.tail ? `\n最新进展：${esc(t.tail)}` : ""}">
      <span class="pill ${esc(t.agent)}">${esc(t.agent)}</span>
      <span class="gnode-title">${esc(t.title || shortId(sid).slice(0, 14))}</span>
      <span class="gnode-sub">${fmtTime(t.started_at)} · ${t.messages} 条</span>
    </div>`;
  }).join("");

  const edgeSvg = edges.filter((e) => pos.has(e.p) && pos.has(e.c)).map((e) => {
    const a = pos.get(e.p), b = pos.get(e.c);
    if (graphVertical) {
      const x1 = a.x + NW / 2, y1 = a.y + NH, x2 = b.x + NW / 2, y2 = b.y;
      const my = (y1 + y2) / 2;
      return `<path d="M ${x1} ${y1} C ${x1} ${my}, ${x2} ${my}, ${x2} ${y2}" marker-end="url(#lg-arrow)"/>
        <text x="${Math.min(x1, x2) + 6}" y="${my - 3}">${esc(e.lt)}</text>`;
    }
    const x1 = a.x + NW, y1 = a.y + NH / 2, x2 = b.x, y2 = b.y + NH / 2;
    const mx = (x1 + x2) / 2;
    return `<path d="M ${x1} ${y1} C ${mx} ${y1}, ${mx} ${y2}, ${x2} ${y2}" marker-end="url(#lg-arrow)"/>
      <text x="${mx}" y="${(y1 + y2) / 2 - 4}" text-anchor="middle">${esc(e.lt)}</text>`;
  }).join("");

  return `<div class="lgraph-wrap">${unresolved.length ? `<div class="meta">部分历史关系包含环路，未按演变顺序排列</div>` : ""}<h2>谱系图（${inGraph.size} 节点 / ${edges.length} 边）
      <button class="btn small graph-orient">${graphVertical ? "横排" : "竖排"}</button>
      <button class="btn small graph-zoom">放大</button></h2>
    <div class="scrollbox lgraph-scroll"><div class="lgraph" style="width:${W}px;height:${H}px">
      <svg class="lgraph-edges" width="${W}" height="${H}">
        <defs><marker id="lg-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="var(--accent)"/></marker></defs>
        ${edgeSvg}
      </svg>
      ${nodeHtml}
    </div></div></div>`;
}

// 图渲染后统一收尾：节点点击跳会话 + 当前节点滚入视口（打开/换向重渲染后都要跑）
function afterGraphRender(scopeSel) {
  document.querySelectorAll(`${scopeSel} .gnode[data-sid]:not(.cur)`).forEach((n) => {
    n.onclick = () => showSession(n.dataset.sid);
  });
  const curNode = document.querySelector(`${scopeSel} .gnode.cur`);
  if (curNode) curNode.scrollIntoView({ inline: "center", block: "nearest" });
}

// ---------------------------------------------------------------- 右键菜单
// 注册制暗色菜单：委托监听（页面重渲染不失效），未注册区域仍用系统菜单。
// 菜单项 = { label, fn, danger } 或 { sep: true }；fn 拿到匹配元素。
const ctxRegistry = [];
const ctxEl = document.createElement("div");
ctxEl.id = "ctx-menu";
ctxEl.style.display = "none";
document.body.appendChild(ctxEl);

function ctxOn(selector, items) { ctxRegistry.push({ selector, items }); }
function closeCtxMenu() { ctxEl.style.display = "none"; }
function openCtxMenu(x, y, items) {
  ctxEl.innerHTML = items
    .map((it, i) => it.sep
      ? `<div class="ctx-sep"></div>`
      : `<div class="ctx-item${it.danger ? " danger" : ""}" data-i="${i}">${it.label}</div>`)
    .join("");
  ctxEl.style.display = "block";
  const r = ctxEl.getBoundingClientRect();
  ctxEl.style.left = Math.max(4, Math.min(x, innerWidth - r.width - 6)) + "px";
  ctxEl.style.top = Math.max(4, Math.min(y, innerHeight - r.height - 6)) + "px";
  ctxEl._items = items;
}
document.addEventListener("contextmenu", (e) => {
  for (const reg of ctxRegistry) {
    const el = e.target.closest(reg.selector);
    if (!el) continue;
    const items = reg.items(el).filter(Boolean);
    if (!items.length) return;
    e.preventDefault();
    openCtxMenu(e.clientX, e.clientY, items);
    return;
  }
});
ctxEl.addEventListener("click", (e) => {
  const item = e.target.closest(".ctx-item");
  if (!item) return;
  const it = ctxEl._items[+item.dataset.i];
  closeCtxMenu();
  it.fn && it.fn();
});
window.addEventListener("click", closeCtxMenu, true);
window.addEventListener("blur", closeCtxMenu);
window.addEventListener("resize", closeCtxMenu);
document.addEventListener("scroll", closeCtxMenu, true);
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeCtxMenu(); });

const ctxCopy = async (text, label = "已复制") => {
  try { await navigator.clipboard.writeText(text); toast(label); }
  catch (e) { toast(String(e)); }
};

// 项目页表格行：卷宗 / 复制 / 访达 / 废弃
ctxOn("#projects-table tr[data-pid]", (el) => {
  const pid = +el.dataset.pid;
  const name = (el.querySelector(".c-name")?.innerText || "").trim();
  const path = el.dataset.path || "";
  return [
    { label: "打开卷宗", fn: () => showProject(pid) },
    { label: "复制项目名", fn: () => ctxCopy(name) },
    path && { label: "复制项目路径", fn: () => ctxCopy(path) },
    path && { label: "打开文件夹", fn: () => invoke("open_in_finder", { path, reveal: false }).catch((e) => toast(String(e))) },
    { sep: true },
    { label: "废弃项目（可恢复）", danger: true, fn: async () => {
        try { await invoke("project_archive", { id: pid }); toast("已废弃，可在已废弃列表恢复"); renderProjects(); }
        catch (e) { toast(String(e)); }
      } },
  ];
});

// 会话页左侧项目卡：只有名字可用（列表数据不含路径）
ctxOn(".sess-proj-item", (el) => [
  { label: "复制项目名", fn: () => ctxCopy((el.querySelector(".n")?.innerText || "").trim()) },
]);

// 对话表格行 / 回收站行：详情 + ID
const ctxSessRow = (el) => {
  const sid = el.dataset.sid;
  return [
    { label: "打开对话详情", fn: () => showSession(sid) },
    { label: "复制对话 ID", fn: () => ctxCopy(sid) },
  ];
};
ctxOn("#sess-table tr[data-sid]", ctxSessRow);
ctxOn("#trash-table tr[data-sid]", ctxSessRow);

// 记忆卡片：内容 / ID / 来源
ctxOn(".memcard", (el) => {
  const items = [{ label: "复制内容", fn: () => ctxCopy(el.querySelector(".content")?.innerText || "") }];
  if (el.dataset.mid) items.push({ label: "复制记忆 ID", fn: () => ctxCopy(el.dataset.mid) });
  const src = el.querySelector("[data-src]");
  if (src) items.push({ label: "跳到来源对话", fn: () => showSession(src.dataset.src) });
  return items;
});

// 带路径的元素（卷宗 artifact 等）：复制 / 访达定位
ctxOn("[data-path]", (el) => [
  { label: "复制路径", fn: () => ctxCopy(el.dataset.path) },
  { label: "定位文件", fn: () => invoke("open_in_finder", { path: el.dataset.path, reveal: true }).catch((e) => toast(String(e))) },
]);


// ---------------------------------------------------------------- 谱系图全屏查看（0.4.2 跟进，2026-09-01 用户反馈：窗口小看不清）
// 克隆已渲染的 .lgraph 进全屏覆盖层；缩放用 CSS zoom（WKWebView 支持且影响布局，
// 滚动范围随缩放自动正确）；Esc/关闭退出，节点仍可点击跳转。
let goEscHandler = null;
function closeGraphOverlay() {
  const ov = document.getElementById("graph-overlay");
  if (ov) ov.remove();
  if (goEscHandler) { document.removeEventListener("keydown", goEscHandler, true); goEscHandler = null; }
}
function openGraphOverlay(graphEl, title) {
  closeGraphOverlay();
  const ov = document.createElement("div");
  ov.id = "graph-overlay";
  ov.innerHTML = `
    <div class="go-bar">
      <span class="go-title">${esc(title || "谱系图 · 滚轮/触控板滚动查看，节点可点跳转，悬停看最新进展")}</span>
      <button class="btn small" data-gz="out">−</button>
      <button class="btn small" data-gz="reset">100%</button>
      <button class="btn small" data-gz="in">＋</button>
      ${graphEl.querySelector(".gnode") ? `<button class="btn small graph-orient">${graphVertical ? "横排" : "竖排"}</button>` : ""}
      <button class="btn small" data-gz="close">关闭（Esc）</button>
    </div>
    <div class="go-scroll"><div class="go-canvas">${graphEl.outerHTML.replaceAll("lg-arrow", "lg-arrow-z")}</div></div>`;
  document.body.appendChild(ov);
  const canvas = ov.querySelector(".go-canvas");
  let zoom = 1;
  ov.querySelectorAll("[data-gz]").forEach((b) => {
    b.onclick = () => {
      if (b.dataset.gz === "close") { closeGraphOverlay(); return; }
      zoom = b.dataset.gz === "reset" ? 1 : Math.min(2.5, Math.max(0.4, zoom + (b.dataset.gz === "in" ? 0.25 : -0.25)));
      canvas.style.zoom = zoom;
    };
  });
  // 当前节点居中
  const cur = canvas.querySelector(".gnode.cur");
  if (cur) cur.scrollIntoView({ inline: "center", block: "center" });
  // Esc 捕获阶段拦截，避免穿透到全局"Esc 关 drawer"
  goEscHandler = (e) => { if (e.key === "Escape") { e.stopPropagation(); closeGraphOverlay(); } };
  document.addEventListener("keydown", goEscHandler, true);
}
// 委托：图标题栏"放大"按钮（会话详情/卷宗/记忆关系图三处共用；data-title 定制覆盖层标题）
document.addEventListener("click", (e) => {
  const z = e.target.closest && e.target.closest("button.graph-zoom");
  if (!z) return;
  const g = z.closest("h2") && z.closest("h2").nextElementSibling && z.closest("h2").nextElementSibling.querySelector(".lgraph");
  if (g) openGraphOverlay(g, z.dataset.title);
});
// 委托：覆盖层内节点点击 → 关覆盖层并跳会话（ext/回收站节点无 data-sid）
document.addEventListener("click", (e) => {
  const n = e.target.closest && e.target.closest("#graph-overlay .gnode[data-sid]:not(.cur)");
  if (n) { closeGraphOverlay(); showSession(n.dataset.sid); }
});

// 委托：横/竖切换——drawer 内的图与全屏覆盖层同步换向重渲染（数据不变，只换布局）
document.addEventListener("click", (e) => {
  const o = e.target.closest && e.target.closest("button.graph-orient");
  if (!o || !lastGraphArgs) return;
  graphVertical = !graphVertical;
  const html = lineageGraphHtml(lastGraphArgs.nodes, lastGraphArgs.edges, lastGraphArgs.currentSid);
  const wrap = document.querySelector("#drawer-content .lgraph-wrap");
  if (wrap) {
    wrap.outerHTML = html;
    afterGraphRender("#drawer-content");
  }
  const canvas = document.querySelector("#graph-overlay .go-canvas");
  if (canvas) {
    const tmp = document.createElement("div");
    tmp.innerHTML = html;
    const g = tmp.querySelector(".lgraph");
    if (g) canvas.innerHTML = g.outerHTML.replaceAll("lg-arrow", "lg-arrow-z");
    const bar = document.querySelector("#graph-overlay .graph-orient");
    if (bar) bar.textContent = graphVertical ? "横排" : "竖排";
    const cur = canvas.querySelector(".gnode.cur");
    if (cur) cur.scrollIntoView({ inline: "center", block: "center" });
  }
});
