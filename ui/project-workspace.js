const STATUS = { open: "未开始", in_progress: "进行中", blocked: "受阻", completed: "已完成", archived: "已归档" };
const KINDS = { message: "消息", task_changed: "任务", artifact: "产物", memory_changed: "记忆", handoff: "交接", document_revision: "文档" };
const RELATIONS = { origin: "起始", continuation: "接续", contribution: "贡献", reference: "参考" };

const option = (value, label, selected, esc) => `<option value="${esc(value)}" ${String(value) === String(selected) ? "selected" : ""}>${esc(label)}</option>`;
const emptyAction = (text, action, label) => `<div class="empty work-empty-action"><span>${text}</span><button class="btn small" data-work-action="${action}">${label}</button></div>`;

export function verificationSummary(task, verifications = []) {
  const countable = verifications.filter((v) => ["test", "manual"].includes(v.kind));
  const current = countable.filter((v) => !v.stale && v.result === "passed");
  if (current.length) return { text: "已验证", className: "verified", verified: true };
  if (countable.some((v) => v.stale && v.result === "passed")) return { text: "验证早于后续修改", className: "stale", verified: false };
  if (countable.some((v) => v.result === "failed")) return { text: "验证失败", className: "failed", verified: false };
  return { text: task?.status === "completed" && task?.status_source === "agent_reported" ? "Agent 自报完成 · 未验证" : "未验证", className: "unverified", verified: false };
}

export function filterTimeline(events, { kind = "", day = "" } = {}) {
  return (events || []).filter((event) => (!kind || event.kind === kind) && (!day || String(event.occurred_at || "").slice(0, 10) === day));
}

function taskCard(task, selected, esc, fmtTime) {
  const verification = task.verification || { text: "未验证", className: "unverified" };
  return `<article class="memcard work-task ${selected === task.id ? "selected" : ""}" data-task-card="${esc(task.id)}">
    <div class="content"><button class="work-task-title" data-task-open="${esc(task.id)}"><code>${esc(task.alias)}</code> ${esc(task.title)}</button></div>
    <div class="meta">
      <span class="pill ${esc(task.status)}">${esc(STATUS[task.status] || task.status)}</span>
      <span>${task.status_source === "user_confirmed" ? "用户确认状态" : "Agent 自报状态"}</span>
      <span class="pill ${esc(verification.className)}">${esc(verification.text)}</span>
      <span>${(task.agents || []).map((a) => esc(a)).join("、") || "尚无关联 Agent"}</span>
      <span>${Number(task.session_count || 0)} 个关联会话</span><span>更新 ${fmtTime(task.updated_at)}</span>
    </div>
  </article>`;
}

function timelineHtml(events, esc, fmtTime) {
  if (!events.length) return '<div class="empty">当前筛选下没有事件</div>';
  return `<div class="work-timeline">${events.map((event) => `<article class="work-event">
    <time>${fmtTime(event.occurred_at || event.observed_at)}</time>
    <div><span class="pill">${esc(KINDS[event.kind] || event.kind)}</span> ${event.agent ? `<span class="pill ${esc(event.agent)}">${esc(event.agent)}</span>` : ""}
      ${event.time_quality === "observed_only" ? '<span class="pill stale" title="只有采集观测时间，不代表工作发生时间">仅观测时间</span>' : event.time_quality === "unknown" ? '<span class="pill stale">时间未知</span>' : ""}
      <p>${esc(event.summary || "（无摘要）")}</p>
      <small>${esc(event.source_table)} · ${esc(event.source_id)}${event.session_id ? ` · <button class="work-link" data-work-session="${esc(event.session_id)}" ${event.source_line_no >= 0 ? `data-line="${event.source_line_no}"` : ""}>查看来源</button>` : ""}</small>
    </div></article>`).join("")}</div>`;
}

export function projectWorkspaceHtml(data, state, esc, fmtTime) {
  const { dossier, tasks, timeline, records, mergeConflicts = [], claims = [] } = data;
  const selected = state.selectedTask;
  const shownTasks = tasks.filter((task) => !state.taskStatus || task.status === state.taskStatus);
  const events = timeline.events;
  const decisions = (dossier.decisions || []).filter((m) => ["decision", "rule"].includes(m.type));
  const pdocs = dossier.project_docs?.docs || [];
  const recordsHtml = records.map((record) => `<article class="memcard"><div class="content"><b>${esc(record.title || record.path || record.record_id)}</b><p>${esc(record.summary || "")}</p></div>
    <div class="meta"><span>${record.origin === "file" ? "项目文档" : "SQLite 交接"}</span><span>${record.time_quality === "observed_only" ? "观测时间" : "发生时间"} ${fmtTime(record.updated_at)}</span>
    ${record.is_pinned ? '<span class="pill confirmed">已固定</span>' : `<button class="btn small" data-record-pin="${esc(record.record_id)}" data-record-kind="${esc(record.record_kind)}">固定为事实源</button>`}</div></article>`).join("");
  const conflictsHtml = mergeConflicts.map((conflict) => `<details class="notice"><summary>${esc(conflict.kind)} · ${esc(conflict.entity_key)}</summary><div class="work-toolbar"><button class="btn small primary" data-merge-resolve="${esc(conflict.id)}" data-choice="local">保留本机</button><button class="btn small" data-merge-resolve="${esc(conflict.id)}" data-choice="incoming">采用导入侧</button></div><div class="work-diff"><h4>本机</h4><pre>${esc(conflict.local || "")}</pre><h4>导入侧</h4><pre>${esc(conflict.incoming || "")}</pre></div></details>`).join("");
  const claimsHtml = claims.map((claim) => `<div class="notice"><span>认领 ${esc(claim.claim_id)} · ${esc(claim.title)} · ${esc(claim.status)}</span><button class="btn small" data-claim-resolve="${esc(claim.claim_id)}">用当前选择的会话认领</button></div>`).join("");
  const docsHtml = pdocs.map((doc) => `<article class="memcard"><div class="content"><code>${esc(doc.name)}</code>${doc.stale ? ' <span class="pill stale">可能已过时</span>' : ""}</div>
    <div class="meta"><span>${doc.exists ? `最后更新 ${fmtTime(doc.last_updated)}` : "文件不存在"}</span>${doc.too_large ? '<span class="pill stale">超过 1 MiB，未备份最新内容</span>' : ""}${doc.collection_error ? `<span class="pill stale">${esc(doc.collection_error)}</span>` : ""}${doc.file_id != null ? `<button class="work-link" data-memory-file="${doc.file_id}">查看修订</button>` : ""}${doc.manual ? `<button class="btn small" data-doc-untrack="${esc(doc.name)}">取消跟踪</button>` : ""}</div></article>`).join("");
  const artifactsHtml = (dossier.artifacts || []).map((a) => `<article class="memcard"><div class="content"><code>${esc(a.path)}</code></div><div class="meta">${esc(a.tool || "文件")}${a.session_id ? ` <button class="work-link" data-work-session="${esc(a.session_id)}">来源会话</button>` : ""}</div></article>`).join("");
  const sessionHtml = (dossier.timeline || []).map((s) => `<tr class="clickable" data-work-session="${esc(s.session_id)}"><td>${fmtTime(s.started_at)}</td><td><span class="pill ${esc(s.agent)}">${esc(s.agent)}</span></td><td>${esc(s.title || "未命名会话")}</td><td>${esc(s.link_type || "")}</td><td>${s.messages}</td></tr>`).join("");
  return `<h1 class="work-title">项目工作台 · ${esc(dossier.project)}</h1><div class="lineage">${esc(dossier.path)}</div>
    <section class="work-section" data-work-section="continue"><h2>1. 继续工作</h2>
      <div class="work-toolbar"><select id="work-current-task" aria-label="当前任务">${tasks.filter((t) => t.status !== "archived").map((t) => option(t.id, `${t.alias} · ${t.title}`, selected, esc)).join("") || '<option value="">尚无项目任务</option>'}</select>
      <button class="btn small primary" id="work-copy-prompt" ${selected ? "" : "disabled"}>复制接续提示词</button><span class="work-hint">只复制文本，不启动 Agent</span></div>
      <div id="work-continuation"><div class="state loading">读取任务节点…</div></div>
      <details class="work-form" id="work-new-task-details"><summary>新建项目任务</summary><div class="searchbar"><input id="work-new-task-title" type="text" placeholder="用户任务，例如：完成导出功能"><button class="btn small primary" id="work-new-task">创建任务</button></div><div id="work-task-candidates"></div><small>Todo / plan 保留在会话“执行步骤”，不会进入此任务板。</small></details>
    </section>
    <section class="work-section" data-work-section="tasks"><h2>2. 任务板 <span>${tasks.length}</span></h2>
      <div class="work-toolbar"><select id="work-task-filter">${["", "open", "in_progress", "blocked", "completed", "archived"].map((s) => option(s, s ? STATUS[s] : "全部状态", state.taskStatus, esc)).join("")}</select><button class="btn small" data-work-action="new-task">新建任务</button></div>
      <div id="work-proposals">${conflictsHtml}${claimsHtml}</div><div id="work-task-list">${shownTasks.map((t) => taskCard(t, selected, esc, fmtTime)).join("") || emptyAction("还没有符合筛选的项目任务。", "new-task", "创建任务")}</div>
    </section>
    <section class="work-section" data-work-section="timeline"><h2>3. 统一时间线 <span>${timeline.events.length}</span></h2>
      <div class="work-toolbar"><select id="work-timeline-kind"><option value="">全部类型</option>${Object.entries(KINDS).map(([k, v]) => option(k, v, state.timelineKind, esc)).join("")}</select><input id="work-timeline-day" type="date" value="${esc(state.timelineDay)}" aria-label="发生日期"><span class="work-hint">跨 Agent 消息、任务、产物、记忆、交接与文档修订</span></div>
      <div id="work-timeline-list">${timelineHtml(events, esc, fmtTime)}</div>${timeline.truncated ? `<div class="work-toolbar"><span class="meta">当前页 ${timeline.events.length} 条</span><button class="btn small" data-timeline-more>载入更早事件</button></div>` : ""}
    </section>
    <section class="work-section" data-work-section="decisions"><h2>4. 决定与规则</h2>
      <div class="work-toolbar"><button class="btn small" data-work-action="new-decision">保存决定 / 规则</button></div>
      <details class="work-form" id="work-decision-details"><summary>记录一条决定或规则</summary><select id="work-decision-type"><option value="decision">决定</option><option value="rule">规则</option></select><textarea id="work-decision-content" rows="5" placeholder="问题、决定与理由、被否方案、复查条件、来源"></textarea><button class="btn small primary" id="work-decision-save">保存为待确认记忆</button></details>
      ${decisions.map((m) => `<article class="memcard"><div class="content">${esc(m.content)}</div><div class="meta"><span class="pill ${esc(m.status)}">${esc(m.status)}</span><span>${esc(m.type)}</span><span>最后确认 / 修改 ${fmtTime(m.updated_at)}</span>${m.source_session_id ? `<button class="work-link" data-work-session="${esc(m.source_session_id)}">来源</button>` : ""}</div></article>`).join("") || emptyAction("还没有项目决定或规则。", "new-decision", "保存第一条决定")}
    </section>
    <section class="work-section" data-work-section="assets"><h2>5. 文档、产物和交接</h2>
      <div class="work-toolbar"><button class="btn small" data-work-action="new-handoff">保存交接</button></div>
      <details class="work-form" id="work-handoff-details"><summary>保存项目交接</summary><input id="work-handoff-title" type="text" placeholder="交接标题"><textarea id="work-handoff-next" rows="3" placeholder="下一步（标题与正文至少填一项）"></textarea><textarea id="work-handoff-state" rows="3" placeholder="当前状态与未解决问题"></textarea><button class="btn small primary" id="work-handoff-save">保存交接</button></details>
      <h3>项目事实源</h3>${recordsHtml || '<div class="empty">暂无项目记录；可保存交接或跟踪项目文档。</div>'}${records.length ? "" : '<button class="btn small" data-work-action="new-handoff">保存交接</button>'}
      <h3>项目文档</h3>${docsHtml || '<div class="empty">暂无项目文档</div>'}<div class="searchbar"><input class="filter-input" id="work-doc-path" type="text" placeholder="相对项目路径，如 docs/plan.md"><button class="btn small" id="work-doc-track">跟踪文档</button></div>
      <h3>文件产物</h3>${artifactsHtml || '<div class="empty">暂无已采集的文件产物</div>'}
    </section>
    <section class="work-section" data-work-section="sessions"><h2>6. 会话证据与谱系</h2>
      <p class="work-hint">会话是任务与事件的证据；执行步骤在会话详情中查看。</p>
      ${sessionHtml ? `<div class="scrollbox"><table><tr><th>开始</th><th>Agent</th><th>摘要</th><th>关系</th><th>消息</th></tr>${sessionHtml}</table></div>` : '<div class="empty">暂无会话证据</div>'}
    </section>`;
}

export function taskContextHtml(context, projectSessions, proposals, esc, fmtTime) {
  const task = context.task;
  const verification = verificationSummary(task, context.verifications || []);
  const linked = new Set((context.sessions || []).map((s) => s.session_id));
  const candidates = projectSessions.sessions.filter((s) => !linked.has(s.session_id));
  const pending = proposals.filter((p) => p.task_id === task.id);
  return `<article class="memcard work-current"><div class="content"><b>${esc(task.alias)} · ${esc(task.title)}</b></div>
    <div class="meta"><span class="pill ${esc(task.status)}">${esc(STATUS[task.status] || task.status)}</span><span class="pill ${esc(verification.className)}">${esc(verification.text)}</span><span>修订 ${task.revision}</span></div>
    <div class="work-toolbar"><label>桌面确认状态 <select id="work-task-status">${Object.entries(STATUS).map(([s, label]) => option(s, label, task.status, esc)).join("")}</select></label><button class="btn small" id="work-task-status-save">更新状态</button></div>
    <p class="work-hint">桌面更新会锁定为用户确认；后续 Agent 冲突状态进入提议，不直接覆盖。</p>
    ${pending.map((p) => `<div class="notice"><span>Agent 提议改为 ${esc(STATUS[p.proposed_status] || p.proposed_status)}</span><button class="btn small primary" data-proposal="${esc(p.id)}" data-accept="true">接受</button><button class="btn small" data-proposal="${esc(p.id)}" data-accept="false">拒绝</button></div>`).join("")}
    <h3>最新交接</h3>${context.handoffs?.length ? `<article class="memcard"><div class="content"><b>${esc(context.handoffs[0].title)}</b><p>${esc(context.handoffs[0].next_steps || context.handoffs[0].state || "")}</p></div><div class="meta">${fmtTime(context.handoffs[0].created_at)}</div></article>` : '<div class="empty">此任务暂无交接</div>'}
    <h3>关联会话</h3>${(context.sessions || []).map((s) => `<div class="work-session-link"><button class="work-link" data-work-session="${esc(s.session_id)}">${esc(s.agent)} · ${esc(s.session_id)}</button><span>${esc(RELATIONS[s.relation] || s.relation)}</span><button class="btn small" data-session-unlink="${esc(s.session_id)}">解除关联</button></div>`).join("") || '<div class="empty">尚未显式关联会话</div>'}
    <div class="work-toolbar"><select id="work-link-session"><option value="">选择同项目会话</option>${candidates.map((s) => option(s.session_id, `${s.agent} · ${s.preview || s.session_id}`, "", esc)).join("")}</select><select id="work-link-relation">${Object.entries(RELATIONS).map(([r, label]) => option(r, label, "continuation", esc)).join("")}</select><button class="btn small" id="work-link-save">显式关联</button></div>
    <h3>验证记录</h3>${(context.verifications || []).map((v) => `<div class="meta"><span class="pill ${v.result === "passed" && !v.stale ? "verified" : "stale"}">${esc(v.result)}${v.stale ? " · 已过期" : ""}</span><span>${esc(v.kind)}</span><span>${esc(v.evidence)}</span><span>${fmtTime(v.created_at)}</span></div>`).join("") || '<div class="empty">没有验证记录；完成状态不等于验证通过。</div>'}
    <details class="work-form"><summary>记录 manual 验证（桌面用户动作）</summary><select id="work-verify-result"><option value="passed">通过</option><option value="failed">失败</option><option value="unknown">未知</option></select><textarea id="work-verify-evidence" rows="3" placeholder="人工核验内容、步骤与实际结果（必填）"></textarea><button class="btn small primary" id="work-verify-save">保存人工验证</button></details>
    <details class="work-form"><summary>添加任务关系</summary><select id="work-task-relation"><option value="depends_on">依赖于</option><option value="continuation">接续</option><option value="related">相关</option><option value="duplicate">重复</option></select><select id="work-task-target"><option value="">目标任务</option>${projectSessions.tasks.filter((t) => t.id !== task.id).map((t) => option(t.id, `${t.alias} · ${t.title}`, "", esc)).join("")}</select><button class="btn small" id="work-task-link-save">确认关系</button></details>
  </article>`;
}

export function createProjectWorkspace(deps) {
  const { invoke, drawer, $, esc, fmtTime, showSession, showMemoryFile, toast, armButton } = deps;
  async function open(pid, preferredTask = "") {
    const request = drawer.begin();
    let data;
    try {
      const [dossier, taskData, timeline, recordData, sessionData] = await Promise.all([
        invoke("project_dossier", { projectId: pid }), invoke("work_tasks", { projectId: pid, status: "all", agent: null }),
        invoke("work_timeline", { projectId: pid, taskId: null, from: null, to: null, kinds: [], cursor: null, limit: 200 }),
        invoke("work_project_records", { projectId: pid }), invoke("sessions", { projectId: pid, limit: null }),
      ]);
      if (!drawer.isCurrent(request)) return;
      data = { dossier, tasks: taskData.tasks, proposals: taskData.proposals, mergeConflicts: taskData.merge_conflicts, claims: taskData.claims, timeline, records: recordData.records, sessions: sessionData.sessions };
    } catch (e) { if (drawer.isCurrent(request)) toast(`读取项目工作台失败：${e}`); return; }
    const state = { selectedTask: data.tasks.some((t) => t.id === preferredTask) ? preferredTask : data.tasks.find((t) => !["completed", "archived"].includes(t.status))?.id || data.tasks[0]?.id || "", taskStatus: "", timelineKind: "", timelineDay: "" };
    let taskRequest = 0, timelineRequest = 0;
    const refresh = () => open(pid, state.selectedTask);
    const timelineRange = () => {
      if (!state.timelineDay) return { from: null, to: null };
      const start = new Date(`${state.timelineDay}T00:00:00`);
      const end = new Date(start); end.setDate(end.getDate() + 1);
      return { from: start.toISOString(), to: end.toISOString() };
    };
    const reloadTimeline = async () => {
      const current = ++timelineRequest;
      try {
        const result = await invoke("work_timeline", { projectId: pid, taskId: null, ...timelineRange(), kinds: state.timelineKind ? [state.timelineKind] : [], cursor: null, limit: 200 });
        if (current === timelineRequest && drawer.isCurrent(request)) { data.timeline = result; paint(); }
      } catch (e) { if (current === timelineRequest) toast(`筛选时间线失败：${e}`); }
    };
    const moreTimeline = async () => {
      const current = ++timelineRequest;
      try {
        const result = await invoke("work_timeline", { projectId: pid, taskId: null, ...timelineRange(), kinds: state.timelineKind ? [state.timelineKind] : [], cursor: data.timeline.next_cursor, limit: 200 });
        if (current !== timelineRequest || !drawer.isCurrent(request)) return;
        const known = new Set(data.timeline.events.map((event) => event.event_key));
        data.timeline = { ...result, events: [...data.timeline.events, ...result.events.filter((event) => !known.has(event.event_key))] };
        paint();
      } catch (e) { if (current === timelineRequest) toast(`读取更早事件失败：${e}`); }
    };
    const paint = () => {
      if (!drawer.isCurrent(request)) return;
      drawer.open(projectWorkspaceHtml(data, state, esc, fmtTime), request);
      bind();
      loadTask();
    };
    const bind = () => {
      const root = $("#drawer-content");
      root.querySelectorAll("[data-work-session]").forEach((el) => { el.onclick = () => showSession(el.dataset.workSession, el.dataset.line == null ? null : Number(el.dataset.line)); });
      root.querySelectorAll("[data-memory-file]").forEach((el) => { el.onclick = () => showMemoryFile(Number(el.dataset.memoryFile)); });
      root.querySelectorAll("[data-task-open]").forEach((el) => { el.onclick = () => { state.selectedTask = el.dataset.taskOpen; paint(); }; });
      root.querySelectorAll("[data-merge-resolve]").forEach((el) => { el.onclick = async () => {
        try { await invoke("work_merge_resolve", { conflictId: el.dataset.mergeResolve, choice: el.dataset.choice }); toast("合并冲突已按你的选择收敛"); refresh(); } catch (e) { toast(String(e)); }
      }; });
      root.querySelectorAll("[data-claim-resolve]").forEach((el) => { el.onclick = async () => {
        const sessionId = $("#work-link-session")?.value;
        if (!sessionId) { toast("请先在当前任务详情中选择要认领的会话"); return; }
        try { await invoke("work_claim_resolve", { claimId: el.dataset.claimResolve, sessionId }); toast("已用显式选择的会话完成认领"); refresh(); } catch (e) { toast(String(e)); }
      }; });
      root.querySelectorAll("[data-work-action]").forEach((el) => { el.onclick = () => {
        const id = { "new-task": "work-new-task-details", "new-decision": "work-decision-details", "new-handoff": "work-handoff-details" }[el.dataset.workAction];
        const details = document.getElementById(id); if (details) { details.open = true; details.scrollIntoView({ block: "nearest" }); details.querySelector("input,textarea")?.focus(); }
      }; });
      $("#work-current-task").onchange = (e) => { state.selectedTask = e.target.value; paint(); };
      $("#work-task-filter").onchange = (e) => { state.taskStatus = e.target.value; paint(); };
      $("#work-timeline-kind").onchange = (e) => { state.timelineKind = e.target.value; reloadTimeline(); };
      $("#work-timeline-day").onchange = (e) => { state.timelineDay = e.target.value; reloadTimeline(); };
      root.querySelectorAll("[data-timeline-more]").forEach((el) => { el.onclick = moreTimeline; });
      $("#work-copy-prompt").onclick = async () => {
        if (!state.selectedTask) return;
        try { const r = await invoke("work_task_continuation", { taskId: state.selectedTask, projectId: pid }); await navigator.clipboard.writeText(r.continuation_prompt); toast("接续提示词已复制；请自行粘贴到新 Agent 会话"); }
        catch (e) { toast(String(e)); }
      };
      $("#work-new-task").onclick = async () => {
        const title = $("#work-new-task-title").value.trim(); if (!title) return;
        try {
          const r = await invoke("work_task_create", { projectId: pid, title, sessionId: null, allowDuplicates: false });
          if (!r.created) {
            $("#work-task-candidates").innerHTML = `<div class="notice"><span>发现相似任务，请先选择已有任务；确认仍需新建可再次点击。</span>${r.candidates.map((t) => `<button class="btn small" data-task-open="${esc(t.id)}">${esc(t.alias)} ${esc(t.title)}</button>`).join("")}<button class="btn small danger" id="work-task-force-create">仍要新建</button></div>`;
            $("#work-task-force-create").onclick = async () => { await invoke("work_task_create", { projectId: pid, title, sessionId: null, allowDuplicates: true }); toast("任务已创建"); refresh(); };
            $("#work-task-candidates").querySelectorAll("[data-task-open]").forEach((el) => { el.onclick = () => { state.selectedTask = el.dataset.taskOpen; paint(); }; });
          } else { toast(`任务 ${r.task.alias} 已创建`); state.selectedTask = r.task.id; refresh(); }
        } catch (e) { toast(String(e)); }
      };
      $("#work-decision-save").onclick = async () => {
        const content = $("#work-decision-content").value.trim(); if (!content) return;
        try { await invoke("memory_add", { projectId: pid, type: $("#work-decision-type").value, content }); toast("已保存为待确认记忆"); refresh(); } catch (e) { toast(String(e)); }
      };
      $("#work-handoff-save").onclick = async () => {
        const title = $("#work-handoff-title").value.trim(), nextSteps = $("#work-handoff-next").value.trim(), stateText = $("#work-handoff-state").value.trim();
        if (!title && !nextSteps && !stateText) { toast("交接标题与正文不能全部为空"); return; }
        try { await invoke("work_handoff_save", { projectId: pid, taskId: state.selectedTask || null, title, nextSteps, state: stateText, done: "", decisions: "", filesChanged: "", openIssues: "", sessionId: null }); toast("交接已保存"); refresh(); } catch (e) { toast(String(e)); }
      };
      $("#work-doc-track").onclick = async () => { try { await invoke("project_doc_track", { projectId: pid, path: $("#work-doc-path").value }); toast("已跟踪，下次采集后备份"); refresh(); } catch (e) { toast(String(e)); } };
      root.querySelectorAll("[data-doc-untrack]").forEach((el) => { el.onclick = async () => { try { await invoke("project_doc_untrack", { projectId: pid, path: el.dataset.docUntrack }); refresh(); } catch (e) { toast(String(e)); } }; });
      root.querySelectorAll("[data-record-pin]").forEach((el) => { el.onclick = async () => { try { await invoke("work_record_pin", { projectId: pid, recordKind: el.dataset.recordKind, recordId: el.dataset.recordPin }); toast("已固定为项目事实源"); refresh(); } catch (e) { toast(String(e)); } }; });
    };
    async function loadTask() {
      const id = state.selectedTask, current = ++taskRequest;
      if (!id) { $("#work-continuation").innerHTML = emptyAction("先创建项目任务，再复制短接续提示词。", "new-task", "创建任务"); return; }
      let context;
      try { context = await invoke("work_task_context", { taskId: id, projectId: pid }); }
      catch (e) { if (current === taskRequest && drawer.isCurrent(request)) $("#work-continuation").textContent = `任务读取失败：${e}`; return; }
      if (current !== taskRequest || !drawer.isCurrent(request) || id !== state.selectedTask) return;
      $("#work-continuation").innerHTML = taskContextHtml(context, { sessions: data.sessions, tasks: data.tasks }, data.proposals, esc, fmtTime);
      bindTask(context);
    }
    function bindTask(context) {
      const task = context.task;
      $("#work-task-status-save").onclick = async () => {
        const status = $("#work-task-status").value; if (status === task.status) return;
        try { await invoke("work_task_update", { taskId: task.id, projectId: pid, title: null, status, expectedRevision: task.revision }); toast("任务状态已由桌面用户确认"); refresh(); } catch (e) { toast(String(e)); }
      };
      $("#work-continuation").querySelectorAll("[data-proposal]").forEach((el) => { el.onclick = async () => { try { await invoke("work_proposal_resolve", { proposalId: el.dataset.proposal, accept: el.dataset.accept === "true" }); refresh(); } catch (e) { toast(String(e)); } }; });
      $("#work-link-save").onclick = async () => {
        const sessionId = $("#work-link-session").value; if (!sessionId) return;
        try { await invoke("work_session_link", { taskId: task.id, sessionId, relation: $("#work-link-relation").value, projectId: pid }); toast("会话已显式关联"); refresh(); } catch (e) { toast(String(e)); }
      };
      $("#work-continuation").querySelectorAll("[data-session-unlink]").forEach((el) => { el.onclick = async () => { try { await invoke("work_session_unlink", { taskId: task.id, sessionId: el.dataset.sessionUnlink, projectId: pid }); toast("已解除关联；相同自动证据不会直接加回"); refresh(); } catch (e) { toast(String(e)); } }; });
      $("#work-verify-save").onclick = async () => {
        const evidence = $("#work-verify-evidence").value.trim(); if (!evidence) { toast("请填写人工验证证据"); return; }
        try { await invoke("work_manual_verification", { taskId: task.id, projectId: pid, result: $("#work-verify-result").value, evidence }); toast("人工验证已记录"); refresh(); } catch (e) { toast(String(e)); }
      };
      $("#work-task-link-save").onclick = async () => {
        const targetTaskId = $("#work-task-target").value; if (!targetTaskId) return;
        try { await invoke("work_task_link", { sourceTaskId: task.id, targetTaskId, relation: $("#work-task-relation").value, confirmed: true, projectId: pid }); toast("任务关系已确认"); } catch (e) { toast(String(e)); }
      };
      $("#work-continuation").querySelectorAll("[data-work-session]").forEach((el) => { el.onclick = () => showSession(el.dataset.workSession); });
    }
    paint();
  }
  return { open };
}
