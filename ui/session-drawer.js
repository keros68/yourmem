// 会话与原生记忆详情共用一个抽屉；每次打开/关闭使旧异步结果失效。
export function createSessionDrawer({ invoke, $, esc, fmtTime, toast, snapScrollboxTables,
  bindCopyButtons, lineageGraphHtml, afterGraphRender, armButton }) {
let generation = 0;
let drawerResume = "";
function begin() { drawerResume = ""; return ++generation; }
const isCurrent = (request) => request === generation;
function openDrawer(html, request = begin()) {
  if (!isCurrent(request)) return;
  $("#drawer-content").innerHTML = html;
  $("#drawer").classList.remove("hidden");
  snapScrollboxTables($("#drawer-content"));
}
function closeDrawer() { begin(); $("#drawer").classList.add("hidden"); }
async function showSession(sessionId, lineNo = null, precompact = false, offset = null, messageId = null) {
  const request = begin();
  let d;
  try {
    d = lineNo != null || offset != null || messageId != null
      ? await invoke("session_window", { sessionId, line: lineNo, offset, messageId, beforeCompact: precompact })
      : await invoke("session", { sessionId, max: 500, line: null, beforeCompact: precompact });
  } catch (e) {
    if (!isCurrent(request)) return;
    toast(`读取对话失败：${e}`);
    return;
  }
  if (!isCurrent(request)) return;
  const s = d.session;
  // 谱系树形图（§7.4，0.4.2）：整条传递闭包家族树，节点可点跳转——
  // 取代旧的 chip 一行链（用户反馈 2026-08-31：一行链要自己脑补结构，没法一眼看全）
  const ltree = d.lineage_tree || { nodes: [], edges: [] };
  const lin = ltree.edges.length
    ? lineageGraphHtml(
        ltree.nodes,
        ltree.edges.map((e) => ({ p: e.parent, c: e.child, lt: e.link_type })),
        sessionId
      )
    : `<div class="lchain"><span class="lnode cur">本对话</span><span style="color:var(--faint)">（暂无可追溯的继承关系）</span></div>`;
  const msgHtml = (m) => `<div class="msg ${esc(m.kind)}" data-line="${m.line_no}" data-message="${m.message_id}"><div class="kind">${esc(m.kind)} · L${m.line_no} · ${fmtTime(m.timestamp)}</div><div class="content">${esc(m.content)}</div>${m.truncated ? `<button class="btn small" data-full-message="${m.message_id}">展开原文</button>` : ""}</div>`;
  // 长对话分块渲染：首屏 150 条，点按钮继续——500+ 条详情不卡的关键。
  // 来源指针跳转（lineNo）指向后块时首屏直接覆盖到目标行（codex 一审：
  // 只渲染前 150 会让定位静默失败，§7.1 跳转要求被分块破坏）。
  const CHUNK = 150;
  let targetIdx = -1;
  if (messageId != null) targetIdx = d.messages.findIndex((m) => m.message_id === messageId);
  else if (lineNo != null) targetIdx = d.messages.findIndex((m) => m.line_no === lineNo);
  let shown = targetIdx >= 0
    ? Math.min(Math.max(CHUNK, targetIdx + 1), d.messages.length)
    : Math.min(CHUNK, d.messages.length);
  let msgs = d.messages.slice(0, shown).map(msgHtml).join("");
  // §4 续聊支持：只展示与复制，不代为执行
  const copyRow = `
    <div class="meta" style="margin:8px 0">
      ${s.resume_command ? `<code>${esc(s.resume_command)}</code> <button class="btn small primary" data-copy="${esc(s.resume_command)}">复制 resume</button>` : ""}
      <button class="btn small" data-copy="${esc(s.native_id || "")}">复制 ID</button>
      ${s.file_path ? `<button class="btn small" data-copy="${esc(s.file_path)}">复制源文件路径</button>` : ""}
    </div>`;
  // 0.3.8 压缩点：标记边界 + 一键切换"压缩前备份"视图（before_compact 切片）
  const compactRow = s.compact_line_no
    ? `<div class="meta compact-row" style="margin:8px 0">
        <span class="pill">⚡ 压缩点 L${esc(s.compact_line_no)}</span>
        此前对话已压缩为摘要，原文已备份。
        <button class="btn small" id="btn-precompact">${precompact ? "返回完整对话" : "只看压缩前"}</button>
      </div>`
    : "";
  // 资产证明卡占位（UI-DESIGN §3，0.4.1）：统计数据异步填，不拖慢 drawer 打开
  const proofRow = `<div class="proof" id="proof-card">
    <div class="proof-title">资产证明 <span style="color:var(--faint);font-weight:400">备份可校验</span></div>
    <div class="proof-body" id="proof-body">加载中…</div>
    <div class="meta" style="margin:6px 0 0" id="proof-actions"></div>
    <div class="proof-result" id="proof-result"></div>
  </div>`;
  const countLine = precompact
    ? `${fmtTime(s.started_at)} → ${fmtTime(s.ended_at)} · 压缩前备份 ${d.total_messages} / ${s.message_count} 条<br>`
    : `${fmtTime(s.started_at)} → ${fmtTime(s.ended_at)} · ${s.message_count} 条消息<br>`;
  openDrawer(`
    <h1 style="font-size:16px">${esc(s.project || "")} · <span class="pill ${esc(s.agent)}">${esc(s.agent)}</span></h1>
    <div class="lineage">${countLine}${lin}</div>
    ${copyRow}
    ${compactRow}
    ${proofRow}
    ${ltree.truncated ? `<div class="meta">仅显示部分谱系节点（上限 ${ltree.node_limit}）</div>` : ""}
    ${d.window_offset != null ? `<div class="searchbar"><button class="btn small" id="context-before" ${d.has_before ? "" : "disabled"}>前文</button><span>${d.messages.length ? d.window_offset + 1 : 0}–${d.window_offset + d.messages.length} / ${d.total_messages}</span><button class="btn small" id="context-after" ${d.has_after ? "" : "disabled"}>后文</button></div>` : ""}
    ${d.window_offset == null ? '<button class="btn small" id="context-browse">浏览全部前后文</button>' : ""}
    ${msgs || '<div class="empty">无消息</div>'}
    ${shown < d.messages.length ? `<div style="text-align:center;margin:10px 0"><button class="btn small" id="msg-more">显示更多（剩余 ${d.messages.length - shown} 条）</button></div>` : ""}
  `, request);
  if (d.window_offset != null) {
    $("#context-before").onclick = () => showSession(sessionId, lineNo, precompact, Math.max(0, d.window_offset - 150), messageId);
    $("#context-after").onclick = () => showSession(sessionId, lineNo, precompact, d.window_offset + 150, messageId);
  }
  const browse = $("#context-browse");
  if (browse) browse.onclick = () => showSession(sessionId, null, precompact, Math.max(0, (d.total_messages || 0) - 150));
  const pcBtn = document.getElementById("btn-precompact");
  if (pcBtn) pcBtn.onclick = () => showSession(sessionId, null, !precompact);
  const more = document.getElementById("msg-more");
  if (more) {
    more.onclick = () => {
      const next = Math.min(shown + CHUNK, d.messages.length);
      more.closest("div").insertAdjacentHTML("beforebegin", d.messages.slice(shown, next).map(msgHtml).join(""));
      shown = next;
      if (shown >= d.messages.length) more.remove();
      else more.textContent = `显示更多（剩余 ${d.messages.length - shown} 条）`;
    };
  }
  // 谱系图节点可点 + 当前节点滚入视口（统一走 afterGraphRender，换向重渲染也用它）
  document.querySelectorAll("#drawer-content [data-full-message]").forEach(btn => {
    btn.onclick = async () => {
      try {
        const r = await invoke("message_content", { sessionId, messageId: Number(btn.dataset.fullMessage) });
        if (!isCurrent(request)) return;
        btn.previousElementSibling.textContent = r.content;
        btn.remove();
      } catch (e) { if (isCurrent(request)) toast(String(e)); }
    };
  });
  afterGraphRender("#drawer-content");
  bindCopyButtons("#drawer-content");
  drawerResume = s.resume_command || ""; // Cmd+C 键盘复制对象（见文件尾键盘区）
  loadProofCard(sessionId, request);
  // 来源指针行号定位：滚动到原始行并高亮（§7.1 点击跳回原始上下文）
  if (lineNo != null || messageId != null) {
    const el = document.querySelector(messageId != null ? `#drawer-content .msg[data-message="${messageId}"]` : `#drawer-content .msg[data-line="${lineNo}"]`);
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "center" });
      el.classList.add("flash");
      setTimeout(() => el.classList.remove("flash"), 2000);
    }
  }
}

async function loadProofCard(sessionId, request) {
  let p;
  try {
    p = await invoke("session_proof", { sessionId });
  } catch (e) {
    if (!isCurrent(request)) return;
    const b = $("#proof-body");
    if (b) b.textContent = `无 vault 归档（${e}）`;
    return;
  }
  if (!isCurrent(request)) return;
  const body = $("#proof-body");
  const actions = $("#proof-actions");
  if (!body || !actions) return;
  body.innerHTML =
    `vault 归档 ${p.lines} 行 · ${p.distinct_objects} 个内容寻址对象（${(p.objects_bytes / 1024).toFixed(1)} KB · 本对话独占 ${p.exclusive_objects} 个）` +
    (p.objects_missing ? ` · <span class="proof-bad">磁盘缺失 ${p.objects_missing} 个对象</span>` : "") +
    `<br>源文件${p.source_exists ? "仍在磁盘" : "已不在磁盘——vault 备份即原件"}：<code>${esc(p.file_path)}</code>`;
  actions.innerHTML = `
    <button class="btn small" id="proof-verify">校验</button>
    <button class="btn small" id="proof-export">导出原件</button>
    ${p.writeback_supported ? '<button class="btn small" id="proof-wb">写回 agent</button>' : ""}`;
  const result = $("#proof-result");
  $("#proof-verify").onclick = async (e) => {
    if (!isCurrent(request)) return;
    e.target.disabled = true;
    result.textContent = "校验中…";
    try {
      const v = await invoke("session_verify", { sessionId });
      if (!isCurrent(request)) return;
      result.innerHTML = v.ok
        ? `<span class="proof-ok">✓ ${v.verified}/${v.objects} 个对象与归档一致（${fmtTime(v.checked_at)}）</span>`
        : `<span class="proof-bad">✗ ${v.failed.length}/${v.objects} 个对象与归档不符或缺失：${esc(v.failed.slice(0, 3).join(", "))}${v.failed.length > 3 ? "…" : ""}</span>`;
    } catch (err) { if (!isCurrent(request)) return; result.textContent = `校验失败：${err}`; }
    e.target.disabled = false;
  };
  $("#proof-export").onclick = async () => {
    if (!isCurrent(request)) return;
    result.textContent = "按 vault 清单重建中…";
    try {
      const r = await invoke("session_export", { sessionId });
      if (!isCurrent(request)) return;
      result.innerHTML = `<span class="proof-ok">✓ 已逐字节重建 ${r.lines} 行</span> <code>${esc(r.path)}</code> <button class="btn small" data-copy="${esc(r.path)}">复制路径</button>`;
      bindCopyButtons("#proof-result");
    } catch (err) { if (!isCurrent(request)) return; result.textContent = `导出失败：${err}`; }
  };
  const wb = $("#proof-wb");
  if (wb) wb.onclick = () => writebackFlow(sessionId, result, request);
}

// 写回（restore-agents 的 UI 承载）：plan 预览 → armed 二次确认 → 执行。
// 目标已存在时确认按钮升级为"覆盖写回（先 .bak）"，执行带 force=true——
// 与 CLI --force 同一语义，.bak 由后端自动做。
async function writebackFlow(sessionId, result, request) {
  if (!isCurrent(request)) return;
  let plan;
  try {
    plan = await invoke("session_writeback_plan", { sessionId });
  } catch (e) { if (!isCurrent(request)) return; result.textContent = `写回预览失败：${e}`; return; }
  if (!isCurrent(request)) return;
  const exists = plan.target_exists;
  result.innerHTML = `
    <div style="margin:6px 0">将按 vault 归档逐字节重建 <b>${plan.vault_lines}</b> 行，写入：<br><code>${esc(plan.will_write[0])}</code>
    ${exists ? '<br><span class="proof-bad">目标已存在——覆盖前自动做 .bak 时间戳备份</span>' : ""}
    <br><span style="color:var(--faint)">一次性写回，非同步；随后在项目目录执行：<code>${esc(plan.resume_command || "—")}</code></span></div>
    <button class="btn small danger" id="wb-confirm"></button>`;
  const btn = $("#wb-confirm");
  armButton(btn, exists ? "覆盖写回（先 .bak）" : "确认写回", "再次点击确认执行", async () => {
    if (!isCurrent(request)) return;
    try {
      const r = await invoke("session_writeback", { sessionId, force: exists });
      if (!isCurrent(request)) return;
      result.innerHTML = `<span class="proof-ok">✓ 已写回 ${r.lines_written} 行</span> <code>${esc(r.restored)}</code>
        ${r.backup ? `<br>原文件备份：<code>${esc(r.backup)}</code>` : ""}
        <br>续聊命令：<code>${esc(r.resume_command || "—")}</code> <button class="btn small" data-copy="${esc(r.resume_command || "")}">复制</button>`;
      bindCopyButtons("#proof-result");
    } catch (err) { if (!isCurrent(request)) return; result.textContent = `写回失败：${err}`; }
  });
}

// 原生 memory 文件备份（DESIGN-0.3 §2）：只读浏览，不提供编辑
async function showMemoryFile(id, revision = null) {
  const request = begin();
  let d;
  try {
    d = await invoke("memory_file_show", { id, revision });
  } catch (e) {
    if (!isCurrent(request)) return;
    toast(`读取 memory 文件失败：${e}`);
    return;
  }
  if (!isCurrent(request)) return;
  const revs = d.revisions
    .map((r) => `<div class="rev clickable" data-rev="${r.id}">
      ${r.id === d.revision.id ? "▸" : ""} r${r.id} · ${r.size}B · ${fmtTime(r.captured_at)}${r.id === d.revision.id ? "（当前查看）" : ""}</div>`)
    .join("");
  openDrawer(`
    <h1 style="font-size:15px;word-break:break-all">${esc(d.file.path)}</h1>
    <div class="lineage"><span class="pill ${esc(d.file.agent)}">${esc(d.file.agent)}</span> ${esc(d.file.scope)} · ${d.revisions.length} 个修订 · 只读备份</div>
    <h2>修订历史</h2>${revs || '<div class="empty">暂无修订</div>'}
    <h2>内容</h2><pre class="memfile">${esc(d.content)}</pre>
  `, request);
  document.querySelectorAll("#drawer-content .rev").forEach((el) => {
    el.onclick = () => showMemoryFile(id, +el.dataset.rev);
  });
}


return { begin, isCurrent, open: openDrawer, close: closeDrawer, showSession, showMemoryFile, get resume() { return drawerResume; } };
}
