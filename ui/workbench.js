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

export function workbenchHtml(digest) {
  const projects = digest?.project_activity || [];
  if (!projects.length) {
    return '<div class="empty workbench-empty">今天还没有可归入项目的对话</div>';
  }
  return `<div class="workbench-grid">${projects.map((p) => {
    const agents = (p.agents || []).map((a) =>
      `<span class="pill ${esc(a.agent)}">${esc(a.agent)} · ${a.sessions}</span>`).join("");
    const activities = (p.activities || []).map((a) => `
      <button class="work-activity" data-work-session="${esc(a.session_id)}">
        <span class="work-time">${fmtTime(a.ended_at || a.started_at)}</span>
        <span class="work-main"><strong>${esc(clipped(a.title, 90) || "未命名对话")}</strong>
          <span>${esc(clipped(a.tail) || "尚无 Agent 回复")}</span></span>
        <span class="work-agent">${esc(a.agent)}</span>
      </button>`).join("");
    const artifacts = (p.artifacts || []).slice(0, 4).map((a) =>
      `<button class="work-link" data-work-session="${esc(a.session_id)}" title="${esc(a.path)}">${esc(clipped(a.path, 80))}</button>`).join("");
    const handoff = p.latest_handoff;
    return `<article class="work-project">
      <header><div><h3>${esc(p.project)}</h3><div class="work-path" title="${esc(p.path)}">${esc(p.path)}</div></div>
        <div class="work-count">${p.sessions} 个对话 · ${p.messages} 条消息</div></header>
      <div class="work-agents">${agents}</div>
      <div class="work-activities">${activities}</div>
      ${(p.artifacts || []).length ? `<div class="work-sub"><b>今日产物</b>${artifacts}${p.artifacts.length > 4 ? `<span class="work-more">另有 ${p.artifacts.length - 4} 个</span>` : ""}</div>` : ""}
      ${(p.open_tasks || []).length ? `<div class="work-sub"><b>未完成</b>${list(p.open_tasks.slice(0, 3).map((t) => clipped(t.content, 120)), "")}</div>` : ""}
      ${handoff ? `<div class="work-sub"><b>最近交接</b><span>${esc(clipped(handoff.next_steps || handoff.title, 160))}</span></div>` : ""}
    </article>`;
  }).join("")}</div>`;
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
