const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function fixture() {
  const elements = new Map(), requests = [];
  const get = id => { if (!elements.has(id)) elements.set(id, {innerHTML: '', textContent: ''}); return elements.get(id); };
  const context = vm.createContext({document: {querySelectorAll: () => []}});
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/project-recall.js'), 'utf8').replace('export async function', 'async function'), context);
  let current = true;
  const promise = context.loadProjectRecall({
    invoke: (command,args) => new Promise((resolve,reject) => requests.push({command,args,resolve,reject})),
    drawer: {isCurrent: () => current}, request: 1, pid: 7, $: get, esc: String, fmtTime: String, showSession() {}, toast() {},
  });
  return {get, requests, promise, close: () => { current = false; }};
}
test('continuation reads the real context shape; baseline is only written after a click', async () => {
  const f = fixture();
  f.requests[0].resolve({latest_handoff: {title: 'handoff', next_steps: 'next action'}, open_tasks: [{content:'pending'}], memories: {confirmed:[{type:'rule', content:'rule evidence'}]}});
  f.requests[1].resolve({status:'unreviewed', head:'abc', tracked_changes:false});
  await f.promise;
  assert.match(f.get('#project-continuation').innerHTML, /next action/);
  assert.match(f.get('#project-continuation').innerHTML, /rule evidence/);
  assert.equal(f.requests.length,2);
  assert.equal(f.requests[1].args.markReviewed,false);
  const save = f.get('#project-reviewed').onclick();
  assert.equal(f.requests[2].args.markReviewed,true);
  f.requests[2].resolve({status:'unchanged',head:'abc',baseline:{head:'abc'}});
  await save;
  assert.match(f.get('#project-review').innerHTML,/与已复查基线一致/);
});
test('obsolete project results and failures cannot replace another drawer', async () => {
  const f=fixture(); f.close();
  f.requests[0].resolve({latest_handoff:{title:'stale'}}); f.requests[1].reject('old error');
  await f.promise;
  assert.equal(f.get('#project-continuation').innerHTML,'');
  assert.equal(f.get('#project-review').textContent,'');
});
test('an unavailable Git repository does not hide continuation information', async () => {
  const f=fixture();
  f.requests[0].resolve({latest_handoff:{title:'available'}}); f.requests[1].resolve({status:'unavailable'});
  await f.promise;
  assert.match(f.get('#project-continuation').innerHTML,/available/);
  assert.match(f.get('#project-review').innerHTML,/无法检测/);
  assert.doesNotMatch(f.get('#project-review').innerHTML,/id="project-reviewed"/);
});
