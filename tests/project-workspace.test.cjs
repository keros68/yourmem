const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

function load(name) {
  const source = fs.readFileSync(`${__dirname}/../ui/${name}`, 'utf8').replaceAll('export function', 'function');
  const context = vm.createContext({});
  vm.runInContext(source + '\nglobalThis.api = { verificationSummary, filterTimeline, projectWorkspaceHtml, taskContextHtml };', context);
  return context.api;
}
const w = load('project-workspace.js');
const esc = String, time = String;

test('project task completion is distinct from verification and later file changes stale it', () => {
  assert.equal(w.verificationSummary({status:'completed',status_source:'agent_reported'}, []).text, 'Agent 自报完成 · 未验证');
  assert.equal(w.verificationSummary({}, [{kind:'test',result:'passed',stale:false}]).verified, true);
  assert.equal(w.verificationSummary({}, [{kind:'test',result:'passed',stale:true}]).text, '验证早于后续修改');
  assert.equal(w.verificationSummary({}, [{kind:'test',result:'failed',stale:false}]).className, 'failed');
  assert.equal(w.verificationSummary({}, [{kind:'external',result:'passed',stale:false}]).verified, false);
});

test('workspace renders task sections and discloses document collection failures', () => {
  const task = {id:'task_1',alias:'P-1',title:'任务',status:'in_progress',revision:1};
  const html = w.projectWorkspaceHtml({
    dossier:{project:'demo',path:'/demo',decisions:[],project_docs:{docs:[{name:'AGENTS.md',exists:true,too_large:true,collection_error:'最新内容尚未备份'}]},artifacts:[],timeline:[]},
    tasks:[task],timeline:{events:[]},records:[],
  }, {selectedTask:'task_1',taskStatus:'',timelineKind:'',timelineDay:''},esc,time);
  for (const text of ['继续工作','任务板','统一时间线','决定与规则','文档、产物和交接','会话证据与谱系','超过 1 MiB','最新内容尚未备份']) assert.match(html,new RegExp(text));
  assert.doesNotMatch(html,/Agent Todo.*data-task-card/);
  assert.match(w.taskContextHtml({task,verifications:[],sessions:[],handoffs:[]},{sessions:[],tasks:[task]},[],esc,time),/更新状态/);
});

test('timeline filters locally without inventing occurrence dates', () => {
  const events = [{kind:'message',occurred_at:'2026-10-25T01:00:00Z'},{kind:'artifact',occurred_at:null}];
  assert.equal(w.filterTimeline(events,{kind:'artifact'}).length,1);
  assert.equal(w.filterTimeline(events,{day:'2026-10-25'}).length,1);
});
