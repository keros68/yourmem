// 卷宗中的接续摘要与项目基线，异步结果服从抽屉代次。
export async function loadProjectRecall({ invoke, drawer, request, pid, $, esc, fmtTime, showSession, toast }) {
  const context = async () => {
    try {
      const d = await invoke("context", { projectId: pid });
      if (!drawer.isCurrent(request)) return;
      const h = d.latest_handoff;
      const tasks = d.open_tasks || [];
      const rules = (d.memories?.confirmed || []).filter(m => m.type === "rule").slice(0, 5);
      $("#project-continuation").innerHTML = `
        ${h ? `<div class="memcard"><div class="content"><b>${esc(h.title)}</b><br>${esc(h.state || "")}${h.next_steps ? `<br>下一步：${esc(h.next_steps)}` : ""}${h.open_issues ? `<br>未解决：${esc(h.open_issues)}` : ""}</div><div class="meta">${fmtTime(h.created_at)}${h.session_id ? ` <button class="btn small" data-resume-source="${esc(h.session_id)}">查看来源</button>` : ""}</div></div>` : '<div class="empty">暂无交接</div>'}
        ${tasks.length ? `<div class="meta">未完成任务${tasks.length > 5 ? "（显示前 5 条）" : ""}</div>${tasks.slice(0, 5).map(t => `<div class="memcard"><div class="content">${esc(t.content)}</div></div>`).join("")}` : ""}
        ${rules.length ? `<div class="meta">已确认规则（最多 5 条）</div>${rules.map(m => `<div class="memcard"><div class="content">${esc(m.content)}</div></div>`).join("")}` : ""}`;
      document.querySelectorAll("#project-continuation [data-resume-source]").forEach(b => { b.onclick = () => showSession(b.dataset.resumeSource); });
    } catch (e) { if (drawer.isCurrent(request)) $("#project-continuation").textContent = `读取失败：${e}`; }
  };
  const review = async (markReviewed = false) => {
    try {
      const r = await invoke("project_review", { projectId: pid, markReviewed });
      if (!drawer.isCurrent(request)) return;
      const labels = { unreviewed: "尚未记录基线", changed: "代码有变化，建议复查项目记忆", unchanged: "与已复查基线一致", unavailable: "无法检测 Git 基线" };
      $("#project-review").innerHTML = `<div class="content">${labels[r.status] || labels.unavailable}</div>
        <div class="meta">检查 Git 提交与已跟踪文件；未跟踪文件及外部资料需另行核对。代码变化不代表记忆失效。</div>
        ${r.head ? `<div class="meta">当前 ${esc(r.head.slice(0, 12))}${r.baseline?.head ? ` · 基线 ${esc(r.baseline.head.slice(0, 12))} · ${fmtTime(r.baseline.reviewed_at)}` : ""}</div>` : ""}
        ${r.head && !r.tracked_changes ? '<button class="btn small" id="project-reviewed" title="确认已复查项目记忆，并记录当前提交">记录基线</button>' : ""}`;
      const b = $("#project-reviewed");
      if (b) b.onclick = () => { b.disabled = true; return review(true); };
    } catch (e) {
      if (!drawer.isCurrent(request)) return;
      $("#project-review").textContent = `复查状态读取失败：${e}`;
      if (markReviewed) toast(String(e));
    }
  };
  await Promise.all([context(), review()]);
}
