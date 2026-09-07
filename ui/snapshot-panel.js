// Daily incremental snapshots. The panel deliberately delegates all storage
// policy and archive work to the native commands.
export function snapshotPanelHtml() {
  return `
      <h2>日常增量快照</h2>
      <div class="memcard" id="snapshot-panel">
        <div class="meta" style="margin-top:0">快照保存压缩数据库，原件在不同日期间共享。请整体保存快照仓库；恢复或跨设备迁移时，先选择快照导出完整备份。</div>
        <div class="searchbar">
          <button class="btn primary" id="snapshot-create">创建快照</button>
          <button class="btn" id="snapshot-refresh">刷新列表</button>
        </div>
        <div id="snapshot-report" role="status" aria-live="polite"></div>
        <div id="snapshot-list" class="scrollbox" style="margin-top:8px"></div>
        <div class="searchbar" style="margin-top:12px">
          <label>保留最近 <input id="snapshot-keep-recent" type="number" min="1" step="1" value="7" style="width:64px" /> 份</label>
          <label>保留最近 <input id="snapshot-keep-monthly" type="number" min="0" step="1" value="6" style="width:64px" /> 个月的月度快照</label>
          <span class="meta">按最近有快照的月份各保留最新一份，再与最近份数合并</span>
          <button class="btn" id="snapshot-cleanup-plan">生成清理计划</button>
          <button class="btn danger hidden" id="snapshot-cleanup-confirm">确认清理</button>
        </div>
        <div id="snapshot-cleanup-report" role="status" aria-live="polite"></div>
      </div>`;
}

export function bindSnapshotPanel({ invoke, root, bundleInput, esc = (s) => String(s ?? "") }) {
  const q = (selector) => root.querySelector(selector);
  const panel = q("#snapshot-panel");
  let busy = false;
  let generation = 0;
  let cleanupSignature = null;
  let cleanupToken = null;

  const bytes = (n) => {
    n = Number(n || 0);
    return n >= 1073741824 ? `${(n / 1073741824).toFixed(2)} GB`
      : n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB`
      : `${(n / 1024).toFixed(1)} KB`;
  };
  const time = (t) => t ? String(t).replace("T", " ").slice(0, 16) : "—";
  const reportError = (el, error) => { el.textContent = `操作失败：${String(error)}`; };
  const setBusy = (value) => {
    busy = value;
    ["#snapshot-create", "#snapshot-refresh", "#snapshot-cleanup-plan", "#snapshot-cleanup-confirm",
      "#snapshot-keep-recent", "#snapshot-keep-monthly"].forEach((s) => { const el = q(s); if (el) el.disabled = value; });
  };
  const current = (ticket) => ticket === generation && panel && (panel.isConnected === undefined || panel.isConnected);
  const invalidateCleanup = () => {
    cleanupSignature = null; cleanupToken = null;
    const button = q("#snapshot-cleanup-confirm");
    if (button) { button.classList.add("hidden"); button.textContent = "确认清理"; }
  };
  const rules = () => ({
    keepRecent: Math.max(1, Number.parseInt(q("#snapshot-keep-recent").value, 10) || 1),
    keepMonthly: Math.max(0, Number.parseInt(q("#snapshot-keep-monthly").value, 10) || 0),
  });
  const renderList = (data) => {
    const list = q("#snapshot-list");
    const rows = data.snapshots || [];
    list.innerHTML = rows.length ? `<table><tr><th>创建时间</th><th class="c-num">压缩数据库</th><th class="c-num">引用原件</th><th class="c-num">原件大小</th><th></th></tr>${rows.map((s) => `
      <tr><td>${esc(time(s.created_at))}</td><td class="c-num">${bytes(s.db_bytes)}</td><td class="c-num">${s.objects ?? 0}</td><td class="c-num">${bytes(s.objects_bytes)}</td>
      <td class="c-act"><button class="btn small" data-snapshot-export="${esc(s.id)}">导出</button></td></tr>`).join("")}</table>`
      : '<div class="meta">暂无快照</div>';
    list.querySelectorAll("[data-snapshot-export]").forEach((button) => {
      button.onclick = () => exportSnapshot(button.dataset.snapshotExport);
    });
  };
  const load = async (force = false, successMessage = null) => {
    if (busy && !force) return;
    invalidateCleanup();
    const ticket = ++generation;
    setBusy(true);
    const report = q("#snapshot-report"); report.textContent = "正在读取快照列表…";
    try {
      const data = await invoke("snapshot_list");
      if (!current(ticket)) return;
      renderList(data);
      const p = data.policy || {};
      if (p.keep_recent != null) q("#snapshot-keep-recent").value = p.keep_recent;
      if (p.keep_monthly != null) q("#snapshot-keep-monthly").value = p.keep_monthly;
      report.textContent = successMessage || (data.root
        ? `快照仓库：${data.root} · 共 ${(data.snapshots || []).length} 份 · 占用 ${bytes(data.repository_bytes)}`
        : "快照列表已更新");
    } catch (error) { if (current(ticket)) reportError(report, error); }
    finally { if (current(ticket)) setBusy(false); }
  };
  async function exportSnapshot(id) {
    if (busy) return;
    const ticket = ++generation; setBusy(true);
    const report = q("#snapshot-report"); report.textContent = "正在导出完整备份…";
    try {
      const result = await invoke("snapshot_export", { id });
      if (!current(ticket)) return;
      if (bundleInput) {
        bundleInput.value = result.path;
        const event = typeof Event === "function" ? new Event("input", { bubbles: true }) : { type: "input" };
        if (bundleInput.dispatchEvent) bundleInput.dispatchEvent(event);
        bundleInput.scrollIntoView({ block: "center" });
      }
      report.textContent = `已导出：${result.path}`;
    } catch (error) { if (current(ticket)) reportError(report, error); }
    finally { if (current(ticket)) setBusy(false); }
  }
  q("#snapshot-create").onclick = async () => {
    if (busy) return;
    invalidateCleanup();
    const ticket = ++generation; setBusy(true);
    const report = q("#snapshot-report"); report.textContent = "正在创建快照…";
    try {
      const result = await invoke("snapshot_create");
      if (!current(ticket)) return;
      const message = `已创建快照：写入 ${result.new_objects || 0} 个新对象，${bytes(result.new_bytes)}`;
      await load(true, message);
    } catch (error) { if (current(ticket)) reportError(report, error); }
    finally { if (current(ticket)) setBusy(false); }
  };
  q("#snapshot-refresh").onclick = () => load();
  ["#snapshot-keep-recent", "#snapshot-keep-monthly"].forEach((s) => { q(s).oninput = invalidateCleanup; });
  q("#snapshot-cleanup-plan").onclick = async () => {
    if (busy) return;
    invalidateCleanup();
    const selected = rules(); const signature = JSON.stringify(selected);
    const ticket = ++generation; setBusy(true);
    const report = q("#snapshot-cleanup-report"); report.textContent = "正在生成清理计划…";
    try {
      const plan = await invoke("snapshot_cleanup_plan", selected);
      if (!current(ticket)) return;
      cleanupSignature = signature; cleanupToken = plan.token;
      const canConfirm = plan.remove_count > 0 || plan.reclaim_bytes > 0;
      report.textContent = canConfirm
        ? `将删除 ${plan.remove_count || 0} 份快照${plan.reclaim_bytes > 0 ? `，并清理 ${bytes(plan.reclaim_bytes)} 个未引用对象` : ""}。再次点击“确认清理”后执行。`
        : "按当前规则无需清理。";
      q("#snapshot-cleanup-confirm").classList.toggle("hidden", !canConfirm);
    } catch (error) { if (current(ticket)) reportError(report, error); }
    finally { if (current(ticket)) setBusy(false); }
  };
  q("#snapshot-cleanup-confirm").onclick = async () => {
    if (busy || !cleanupToken) return;
    const selected = rules();
    if (JSON.stringify(selected) !== cleanupSignature) { invalidateCleanup(); return; }
    const ticket = ++generation; setBusy(true);
    const report = q("#snapshot-cleanup-report"); report.textContent = "正在清理快照…";
    try {
      const result = await invoke("snapshot_cleanup", { ...selected, token: cleanupToken });
      if (!current(ticket)) return;
      invalidateCleanup(); report.textContent = `已清理 ${result.removed || 0} 份快照，释放 ${bytes(result.reclaimed_bytes)}`;
      await load(true);
    } catch (error) { if (current(ticket)) reportError(report, error); }
    finally { if (current(ticket)) setBusy(false); }
  };
  load();
  return { reload: load };
}
