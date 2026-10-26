export function filterPreferences(memories, { keyword = "", source = "", order = "recent" } = {}) {
  const query = keyword.trim().toLowerCase();
  return (memories || []).filter((m) => (!query || `${m.content} ${m.id}`.toLowerCase().includes(query))
    && (!source || (source === "manual" ? !m.source_agent : m.source_agent === source)))
    .sort((a, b) => (order === "oldest" ? 1 : -1) * String(a.updated_at).localeCompare(String(b.updated_at)));
}

const option = (value, label, current, esc) => `<option value="${esc(value)}" ${String(value) === String(current) ? "selected" : ""}>${esc(label)}</option>`;

export function preferencesHtml(memories, projects, state, esc, fmtTime) {
  const rows = filterPreferences(memories, state);
  const agents = [...new Set(memories.map((m) => m.source_agent).filter(Boolean))].sort();
  return `<h1>偏好治理 <span class="en">Preferences</span></h1>
    <div class="work-toolbar"><button class="btn small" data-governance-view="list">← 全部记忆</button><button class="btn small" data-governance-view="labels">显式标签</button><button class="btn small primary" data-preference-new>新增偏好</button></div>
    <div class="memcard governance-filters"><input class="filter-input" id="pref-keyword" type="search" placeholder="搜索内容或 ID" value="${esc(state.keyword)}">
      <select id="pref-scope"><option value="">全局 + 项目</option>${option("global", "仅全局", state.scope, esc)}${option("project", "仅项目", state.scope, esc)}</select>
      <select id="pref-project"><option value="">全部项目</option>${projects.map((p) => option(p.id, p.name, state.project, esc)).join("")}</select>
      <select id="pref-source"><option value="">全部来源</option>${option("manual", "桌面 / 手工", state.source, esc)}${agents.map((a) => option(a, a, state.source, esc)).join("")}</select>
      <select id="pref-status">${["all", "confirmed", "suggested", "superseded", "archived"].map((s) => option(s, { all: "全部状态", confirmed: "已确认", suggested: "待确认", superseded: "已取代", archived: "已失效" }[s], state.status, esc)).join("")}</select>
      <select id="pref-order">${option("recent", "最后确认 / 修改：新到旧", state.order, esc)}${option("oldest", "最后确认 / 修改：旧到新", state.order, esc)}</select></div>
    <details class="work-form" id="preference-new" ${state.creating ? "open" : ""}><summary>新增偏好</summary><div class="searchbar"><select id="pref-new-project"><option value="">全局</option>${projects.map((p) => option(p.id, `项目：${p.name}`, state.project, esc)).join("")}</select></div><textarea id="pref-new-content" rows="4" placeholder="明确、可复查的个人偏好"></textarea><button class="btn small primary" id="pref-new-save">保存偏好</button></details>
    <p class="work-hint">最后确认 / 修改使用 updated_at。修改会保存新版本并保留取代链；归档表示失效，历史仍可筛选。</p>
    <div id="preference-list">${rows.map((m) => state.editing === m.id ? `<article class="memcard" data-pref-card="${esc(m.id)}"><textarea id="pref-edit-content" rows="6">${esc(m.content)}</textarea><div class="meta"><button class="btn small primary" data-pref-edit-save="${esc(m.id)}" data-updated="${esc(m.updated_at)}">保存新版本</button><button class="btn small" data-pref-cancel>取消</button><span>旧版本将标记为 superseded</span></div></article>` : `<article class="memcard" data-pref-card="${esc(m.id)}"><div class="content">${esc(m.content)}</div>
      <div class="meta"><span class="pill ${esc(m.status)}">${esc(m.status)}</span><span>${esc(m.scope)}${m.project ? ` · ${esc(m.project)}` : ""}</span><span>来源 ${esc(m.source_agent || "桌面 / 手工")}</span><span>最后确认 / 修改 ${fmtTime(m.updated_at)}</span>${m.source_session_id ? `<button class="work-link" data-pref-source="${esc(m.source_session_id)}" data-line="${m.source_line_no ?? ""}">原始来源</button>` : ""}</div>
      <div class="work-toolbar"><button class="btn small" data-pref-edit="${esc(m.id)}">修改</button>${m.status === "suggested" ? `<button class="btn small primary" data-pref-action="confirm" data-id="${esc(m.id)}" data-updated="${esc(m.updated_at)}">确认</button>` : ""}<button class="btn small" data-pref-supersede="${esc(m.id)}">取代</button><button class="btn small" data-pref-action="archive" data-id="${esc(m.id)}" data-updated="${esc(m.updated_at)}">失效</button><code>${esc(m.id)}</code></div>
      ${state.superseding === m.id ? `<div class="work-toolbar"><input class="filter-input" id="pref-supersede-target" type="text" placeholder="输入取代它的完整 memory ID"><button class="btn small primary" data-pref-supersede-save="${esc(m.id)}" data-updated="${esc(m.updated_at)}">确认取代</button><button class="btn small" data-pref-cancel>取消</button></div>` : ""}</article>`).join("") || '<div class="empty">当前筛选下没有偏好；可调整筛选或新增偏好。</div>'}</div>`;
}

export function labelsHtml(labels, projects, state, esc, fmtTime) {
  return `<h1>显式标签 <span class="en">Labels</span></h1>
    <div class="work-toolbar"><button class="btn small" data-governance-view="list">← 全部记忆</button><button class="btn small" data-governance-view="preferences">偏好治理</button><button class="btn small" id="labels-export">导出 JSON</button></div>
    <p class="work-hint">标签只由用户显式创建与关联，不根据使用次数自动生成。合并保留目标标签及全部关联；删除不影响记忆本体。</p>
    <section class="memcard"><h2>创建标签</h2><div class="searchbar"><input id="label-new-name" type="text" placeholder="标签名称"><select id="label-new-scope">${option("global", "全局", state.scope, esc)}${option("project", "项目", state.scope, esc)}</select><select id="label-new-project"><option value="">选择项目</option>${projects.map((p) => option(p.id, p.name, state.project, esc)).join("")}</select><button class="btn small primary" id="label-create">创建</button></div></section>
    <section class="memcard"><h2>关联记忆</h2><div class="searchbar"><input id="label-memory-id" type="text" placeholder="完整 memory ID"><select id="label-attach-id">${labels.map((l) => option(l.id, `${l.name} · ${l.project || "全局"}`, state.attachLabel, esc)).join("")}</select><button class="btn small primary" id="label-attach" ${labels.length ? "" : "disabled"}>关联</button></div><small>记忆 ID 可在记忆或偏好卡片复制；关联是显式用户动作。</small></section>
    <div id="labels-list">${labels.map((l) => `<article class="memcard"><div class="content"><b>${esc(l.name)}</b> <span class="pill">${esc(l.scope)}</span></div><div class="meta"><span>${esc(l.project || "全部项目")}</span><span>${l.memory_count} 条关联记忆</span><span>创建 ${fmtTime(l.created_at)}</span><code>${esc(l.id)}</code></div>
      <div class="work-toolbar"><input data-label-rename-input="${esc(l.id)}" value="${esc(l.name)}" aria-label="重命名 ${esc(l.name)}"><button class="btn small" data-label-rename="${esc(l.id)}">重命名</button><select data-label-merge-target="${esc(l.id)}"><option value="">合并到…</option>${labels.filter((target) => target.id !== l.id).map((target) => option(target.id, target.name, "", esc)).join("")}</select><button class="btn small" data-label-merge="${esc(l.id)}">合并</button><button class="btn small danger" data-label-delete="${esc(l.id)}">删除</button></div></article>`).join("") || '<div class="empty">还没有显式标签；先在上方创建。</div>'}</div>`;
}

export function createAssetGovernance({ invoke, $, esc, fmtTime, toast, armButton, showSession, showMemory }) {
  let request = 0;
  const states = {
    preferences: { keyword: "", scope: "", project: "", source: "", status: "all", order: "recent", editing: "", superseding: "", creating: false },
    labels: { scope: "global", project: "", attachLabel: "" },
  };
  async function render(view) {
    const current = ++request;
    const state = states[view];
    let memories = [], labels = [], projects = [];
    try {
      if (view === "preferences") {
        const [md, pd] = await Promise.all([
          invoke("memories", { project: state.project || null, status: state.status, type: "preference", scope: state.scope || null, agent: null, limit: 0 }),
          invoke("projects"),
        ]);
        memories = md.memories; projects = pd.projects;
      } else {
        const [ld, pd] = await Promise.all([invoke("labels"), invoke("projects")]);
        labels = ld.labels; projects = pd.projects;
      }
    } catch (e) { if (current === request) $("#page-memory").innerHTML = `<div class="state error">读取治理视图失败：${esc(String(e))}</div>`; return; }
    if (current !== request) return;
    const paint = () => {
      $("#page-memory").innerHTML = view === "preferences" ? preferencesHtml(memories, projects, state, esc, fmtTime) : labelsHtml(labels, projects, state, esc, fmtTime);
      bind();
    };
    const bind = () => {
      const root = $("#page-memory");
      root.querySelectorAll("[data-governance-view]").forEach((el) => { el.onclick = () => {
        if (el.dataset.governanceView === "list") showMemory(); else render(el.dataset.governanceView);
      }; });
      if (view === "labels") {
        $("#label-new-scope").onchange = (e) => { state.scope = e.target.value; };
        $("#label-new-project").onchange = (e) => { state.project = e.target.value; };
        $("#label-create").onclick = async () => {
          const name = $("#label-new-name").value.trim(), scope = $("#label-new-scope").value, projectId = $("#label-new-project").value;
          if (!name || (scope === "project" && !projectId)) { toast("项目标签需要名称和所属项目"); return; }
          try { await invoke("label_create", { name, scope, projectId: scope === "project" ? Number(projectId) : null }); toast("标签已创建"); render(view); } catch (e) { toast(String(e)); }
        };
        $("#label-attach").onclick = async () => {
          const memoryId = $("#label-memory-id").value.trim(), labelId = $("#label-attach-id").value;
          if (!memoryId || !labelId) return;
          try { await invoke("label_attach", { memoryId, labelId }); toast("标签已关联"); render(view); } catch (e) { toast(String(e)); }
        };
        root.querySelectorAll("[data-label-merge]").forEach((el) => { el.onclick = async () => {
          const toLabelId = root.querySelector(`[data-label-merge-target="${el.dataset.labelMerge}"]`).value;
          if (!toLabelId) { toast("请选择目标标签"); return; }
          try { await invoke("label_merge", { fromLabelId: el.dataset.labelMerge, toLabelId }); toast("标签已合并"); render(view); } catch (e) { toast(String(e)); }
        }; });
        root.querySelectorAll("[data-label-rename]").forEach((el) => { el.onclick = async () => {
          const name = root.querySelector(`[data-label-rename-input="${el.dataset.labelRename}"]`).value.trim();
          if (!name) { toast("标签名不能为空"); return; }
          try { await invoke("label_rename", { labelId: el.dataset.labelRename, name }); toast("标签已重命名"); render(view); } catch (e) { toast(String(e)); }
        }; });
        root.querySelectorAll("[data-label-delete]").forEach((el) => { armButton(el, "删除", "确认删除标签？记忆保留", async () => { await invoke("label_delete", { labelId: el.dataset.labelDelete }); toast("标签已删除"); render(view); }); });
        $("#labels-export").onclick = async () => {
          try { const r = await invoke("labels_export"); toast(`标签已导出：${r.path}`); await navigator.clipboard.writeText(r.path); } catch (e) { toast(String(e)); }
        };
        return;
      }
      const rerun = () => render(view);
      $("#pref-keyword").onchange = (e) => { state.keyword = e.target.value; paint(); };
      $("#pref-keyword").onsearch = (e) => { state.keyword = e.target.value; paint(); };
      $("#pref-scope").onchange = (e) => { state.scope = e.target.value; rerun(); };
      $("#pref-project").onchange = (e) => { state.project = e.target.value; rerun(); };
      $("#pref-source").onchange = (e) => { state.source = e.target.value; paint(); };
      $("#pref-status").onchange = (e) => { state.status = e.target.value; rerun(); };
      $("#pref-order").onchange = (e) => { state.order = e.target.value; paint(); };
      $("[data-preference-new]").onclick = () => { state.creating = true; paint(); };
      $("#pref-new-save").onclick = async () => {
        const content = $("#pref-new-content").value.trim(); if (!content) return;
        try { await invoke("memory_add", { projectId: $("#pref-new-project").value ? Number($("#pref-new-project").value) : null, type: "preference", content }); state.creating = false; toast("偏好已保存"); rerun(); } catch (e) { toast(String(e)); }
      };
      root.querySelectorAll("[data-pref-edit]").forEach((el) => { el.onclick = () => { state.editing = el.dataset.prefEdit; state.superseding = ""; paint(); }; });
      root.querySelectorAll("[data-pref-supersede]").forEach((el) => { el.onclick = () => { state.superseding = el.dataset.prefSupersede; state.editing = ""; paint(); }; });
      root.querySelectorAll("[data-pref-cancel]").forEach((el) => { el.onclick = () => { state.editing = state.superseding = ""; paint(); }; });
      root.querySelectorAll("[data-pref-edit-save]").forEach((el) => { el.onclick = async () => {
        const content = $("#pref-edit-content").value.trim(); if (!content) return;
        try { await invoke("preference_revise", { id: el.dataset.prefEditSave, content, expectedUpdatedAt: el.dataset.updated }); state.editing = ""; toast("偏好新版本已保存"); rerun(); } catch (e) { toast(String(e)); }
      }; });
      root.querySelectorAll("[data-pref-supersede-save]").forEach((el) => { el.onclick = async () => {
        const supersededBy = $("#pref-supersede-target").value.trim(); if (!supersededBy) return;
        try { await invoke("update_memory", { id: el.dataset.prefSupersedeSave, action: "supersede", supersededBy, expectedUpdatedAt: el.dataset.updated }); state.superseding = ""; toast("已记录取代关系"); rerun(); } catch (e) { toast(String(e)); }
      }; });
      root.querySelectorAll("[data-pref-action]").forEach((el) => { el.onclick = async () => {
        try { await invoke("update_memory", { id: el.dataset.id, action: el.dataset.prefAction, supersededBy: null, expectedUpdatedAt: el.dataset.updated }); toast(el.dataset.prefAction === "archive" ? "偏好已失效，历史保留" : "偏好已确认"); rerun(); } catch (e) { toast(String(e)); }
      }; });
      root.querySelectorAll("[data-pref-source]").forEach((el) => { el.onclick = () => showSession(el.dataset.prefSource, el.dataset.line ? Number(el.dataset.line) : null); });
    };
    paint();
  }
  return { render };
}
