const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const context = vm.createContext({ Set });
vm.runInContext(
  fs.readFileSync(path.join(__dirname, '../ui/workbench.js'), 'utf8')
    .replace(/export function /g, 'function ') +
    '\nglobalThis.api = { todayOverviewHtml, activityPageHtml, aiSummaryHtml };',
  context,
);

const digest = { day: '2026-09-09', sessions: 2, messages: 20, artifacts_added: 1,
  open_tasks: [{ content: 'run acceptance test', project: 'alpha', agent: 'codex' }],
  recent_handoffs: [{ project: 'alpha', created_at: '2026-09-09T10:30:00Z', next_steps: 'publish after verification' }],
  project_activity: [{
    project_id: 7, project: 'alpha', path: 'D:/alpha', sessions: 2, messages: 20,
    agents: [{ agent: 'codex', sessions: 2 }],
    activities: [{ session_id: 's1', agent: 'codex', ended_at: '2026-09-09T10:20:00Z', title: '<fix>', tail: 'implemented feature' }],
    artifacts: [{ session_id: 's1', agent: 'codex', path: 'src/main.rs', tool: 'Write', created_at: '2026-09-09T10:10:00Z' }],
    open_tasks: [{ content: 'run acceptance test', project: 'alpha' }],
    latest_handoff: { title: 'handoff', created_at: '2026-09-09T10:30:00Z', next_steps: 'publish after verification' },
  }] };

test('Today stays compact and sends users to the separate Activity page', () => {
  const html = context.api.todayOverviewHtml(digest);
  assert.match(html, /alpha/);
  assert.match(html, /查看动态/);
  assert.match(html, /data-work-session="s1"/);
  assert.match(html, /&lt;fix&gt;/);
  assert.match(html, /implemented feature/);
  assert.match(html, /run acceptance test/);
  assert.match(html, /publish after verification/);
  assert.doesNotMatch(html, /工作账本/);
});

test('Activity supports project, Agent and detail-tab navigation', () => {
  const html = context.api.activityPageHtml(digest, { project: 'all', agent: 'codex', selected: '7', tab: 'artifacts', today: '2026-09-09' });
  assert.match(html, />动态</);
  assert.match(html, /Daily activity/);
  assert.match(html, /全部项目/);
  assert.match(html, /全部 Agent/);
  assert.match(html, /data-activity-project="7"/);
  assert.match(html, /data-activity-tab="artifacts"/);
  assert.match(html, /src\/main\.rs/);
  assert.match(html, /data-work-session="s1"/);
  assert.match(html, /id="activity-next"[^>]*disabled/);
});

test('AI suggestions show source links and require an explicit save', () => {
  const html = context.api.aiSummaryHtml({ source_projects: 1, source_sessions: 2, input_chars: 321,
    result: { overview: 'one day', projects: [{ project: 'alpha', status: 'mixed', summary: 'progress',
      completed: ['A'], in_progress: ['B'], blocked: [], decisions: ['C'], next_steps: ['D'], sources: ['s1', 's2'] }] }
  }, new Set());
  assert.match(html, /AI 整理建议/);
  assert.match(html, /有进展也有遗留/);
  assert.match(html, /data-ai-source="s1"/);
  assert.match(html, /data-ai-save="0"/);
  assert.match(html, /保存为项目摘要/);
  assert.doesNotMatch(html, /disabled/);

  const saved = context.api.aiSummaryHtml({ result: { projects: [{ project: 'alpha', sources: ['s1'] }] } }, new Set([0]));
  assert.match(saved, /已保存/);
  assert.match(saved, /disabled/);
});
