const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(`${__dirname}/../ui/asset-governance.js`, 'utf8').replaceAll('export function', 'function');
const context = vm.createContext({});
vm.runInContext(source + '\nglobalThis.api = {filterPreferences, preferencesHtml, labelsHtml};', context);
const api = context.api;

test('preferences filter source and sort without mutating the source list', () => {
  const memories = [
    {id:'a',content:'first',updated_at:'2026-01-01',source_agent:'claude'},
    {id:'b',content:'second',updated_at:'2026-02-01'},
  ];
  assert.deepEqual(Array.from(api.filterPreferences(memories,{source:'manual'}),m=>m.id),['b']);
  assert.deepEqual(Array.from(api.filterPreferences(memories,{order:'oldest'}),m=>m.id),['a','b']);
  assert.equal(memories[0].id,'a');
  const html=api.preferencesHtml(memories,[],{keyword:'',scope:'',project:'',source:'',status:'all',order:'recent'},String,String);
  assert.match(html,/偏好治理/);
  assert.match(html,/保存新版本/);
});

test('labels expose merge, export and non-destructive deletion',()=>{
  const html=api.labelsHtml([{id:'l1',name:'research',scope:'global',memory_count:2,created_at:'2026-01-01'}],[],{scope:'global',project:'',attachLabel:'l1'},String,String);
  assert.match(html,/导出 JSON/);
  assert.match(html,/合并/);
  assert.match(html,/删除/);
  assert.match(html,/删除不影响记忆本体/);
});
