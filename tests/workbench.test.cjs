const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const context = vm.createContext({ Set });
vm.runInContext(
  fs.readFileSync(path.join(__dirname, '../ui/workbench.js'), 'utf8')
    .replace(/export function /g, 'function ') +
    '\nglobalThis.api = { workbenchHtml, aiSummaryHtml };',
  context,
);

test('workbench groups project activity and keeps every item linked to its session', () => {
  const html = context.api.workbenchHtml({ project_activity: [{
    project: 'alpha', path: 'D:/alpha', sessions: 2, messages: 20,
    agents: [{ agent: 'codex', sessions: 2 }],
    activities: [{ session_id: 's1', agent: 'codex', ended_at: '2026-09-08T10:20:00Z', title: '<fix>', tail: 'implemented feature' }],
    artifacts: [{ session_id: 's1', path: 'src/main.rs' }],
    open_tasks: [{ content: 'run acceptance test' }],
    latest_handoff: { next_steps: 'publish after verification' },
  }] });
  assert.match(html, /alpha/);
  assert.match(html, /codex · 2/);
  assert.match(html, /data-work-session="s1"/);
  assert.match(html, /&lt;fix&gt;/);
  assert.match(html, /implemented feature/);
  assert.match(html, /run acceptance test/);
  assert.match(html, /publish after verification/);
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
