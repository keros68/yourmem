const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function app() {
  const nodes = new Map(), lists = new Map(), requests = [], notices = [];
  const make = () => ({ innerHTML: '', textContent: '', value: '', dataset: {}, style: {},
    focus() {}, scrollIntoView() {}, remove() {}, addEventListener() {}, classList: { add() {}, remove() {} }, querySelectorAll: () => [] });
  const get = (key) => { if (!nodes.has(key)) nodes.set(key, make()); return nodes.get(key); };
  const context = {
    document: { querySelector: get, querySelectorAll: (sel) => lists.get(sel) || [], addEventListener() {},
      createElement: make, body: { appendChild: (node) => notices.push(node) } },
    window: { addEventListener() {}, __TAURI__: { core: { invoke: (command, args) => new Promise((resolve, reject) => requests.push({ command, args, resolve, reject })) } } },
    setTimeout() {},
  };
  vm.createContext(context);
  const ui = (name) => fs.readFileSync(path.join(__dirname, '../ui', name), 'utf8');
  vm.runInContext(ui('session-drawer.js').replace('export function', 'function'), context);
  vm.runInContext(ui('project-recall.js').replace('export async function', 'async function'), context);
  vm.runInContext(ui('graph-layout.js').replace('export function', 'function'), context);
  const bundleCode = ui('app.js').slice(ui('app.js').indexOf('  let bundleBusy = false'), ui('app.js').indexOf('  $("#auto-purge").onchange'));
  vm.runInContext(ui('app.js').replace(/^import .*;\r?\n/gm, '') + '\nfunction bindBundleForTest() { ' + bundleCode + ' }\nglobalThis.testApp = { bindBundleForTest, renderSearch, renderMemory, armButton, memoryGraphHtml, lineageGraphHtml, bindMemoryGraph, setMemoryView: v => { memView = v; } };', context);
  return { ...context.testApp, get, lists, requests, notices };
}

test('app initializes with the extracted drawer and searches using the shared agent list', async () => {
  const f = app();
  await f.renderSearch();
  assert.match(f.get('#page-search').innerHTML, /value="hermes"/);
  assert.ok(f.requests.some(r => r.command === 'first_run_state'));
});

test('search ignores stale results and reports current errors', async () => {
  const f = app(); await f.renderSearch();
  f.get('#q').value = 'A'; const a = f.get('#q-go').onclick();
  const first = f.requests.at(-1);
  f.get('#q').value = 'B'; const b = f.get('#q-go').onclick();
  f.requests.at(-1).resolve({ results: [{ snippet: 'B result' }] }); await b;
  first.resolve({ results: [{ snippet: 'A result' }] }); await a;
  assert.match(f.get('#q-results').innerHTML, /B result/);
  assert.doesNotMatch(f.get('#q-results').innerHTML, /A result/);
  const failed = f.get('#q-go').onclick();
  f.requests.at(-1).reject('search failed'); await failed;
  assert.equal(f.notices.at(-1).textContent, 'search failed');
});

test('armed action still requires two clicks and reports rejection', async () => {
  const f = app(), button = f.get('#action');
  let calls = 0;
  f.armButton(button, 'delete', 'confirm', async () => { calls++; throw new Error('failed'); });
  await button.onclick(); assert.equal(calls, 0);
  await button.onclick(); assert.equal(calls, 1);
  assert.match(f.notices.at(-1).textContent, /failed/);
});

test('memory graph lays out converging predecessors and exposes missing or cyclic history', () => {
  const f=app();
  const nodes=[{id:'A',superseded_by:'B'}, {id:'B',superseded_by:'C'}, {id:'D',superseded_by:'C'}, {id:'C'}]
    .map(m => ({content:m.id,created_at:'2026-09-06',status:m.superseded_by?'superseded':'confirmed',...m}));
  const html=f.memoryGraphHtml(nodes);
  const x = id => Number(html.match(new RegExp(`data-mid="${id}"[^>]*left:(\\d+)px`))[1]);
  assert.ok(x('C')>x('B')); assert.ok(x('B')>x('A'));
  assert.match(f.memoryGraphHtml([{...nodes[0],superseded_by:'missing'}]),/目标不可用/);
  assert.match(f.memoryGraphHtml([{...nodes[0],superseded_by:'B'}, {...nodes[1],superseded_by:'A'}]),/包含环路/);
});

test('lineage cycle warning appears once even when every node is present', () => {
  const f=app();
  const html=f.lineageGraphHtml([{session_id:'A'}, {session_id:'B'}], [{p:'A',c:'B'}, {p:'B',c:'A'}]);
  assert.equal((html.match(/包含环路/g)||[]).length,1);
});

test('rendering memory sources does not overwrite graph highlighting handlers', async () => {
  const f=app(); f.setMemoryView('graph');
  const mems=[{id:'A',superseded_by:'B'},{id:'B',superseded_by:'C'},{id:'D',superseded_by:'C'},{id:'C'}]
    .map(m=>({...m,content:m.id,status:'confirmed'}));
  const nodes=mems.map(m => {
    const classes=new Set();
    return {dataset:{mid:m.id,src:'',sup:m.superseded_by||''},classes,classList:{add:x=>classes.add(x),remove:x=>classes.delete(x)}};
  });
  f.get('#page-memory .lgraph').querySelectorAll = selector => selector === '.mnode-g.hl' ? nodes.filter(n=>n.classes.has('hl')) : nodes;
  // Broad source selection would include graph nodes and replace their onclick.
  f.lists.set('#page-memory [data-src]',nodes);
  const pending=f.renderMemory();
  const calls=f.requests.slice(-3);
  assert.equal(calls[0].args.status,'all'); assert.equal(calls[0].args.agent,null); assert.equal(calls[0].args.type,null);
  calls[0].resolve({memories:mems}); calls[1].resolve({memories:[]}); calls[2].resolve({memory_files:[]});
  await pending;
  const count=f.requests.length;
  nodes[3].onclick({target:{classList:{contains:()=>false}}});
  assert.equal(nodes.filter(n=>n.classes.has('hl')).length,4);
  assert.equal(f.requests.length,count);
});


test('bundle controls prevent duplicate work and bind confirmation to the current source and plan', async () => {
  const f = app(); f.bindBundleForTest();
  const input = f.get('#bundle-path'), restore = f.get('#bundle-restore');
  const report = { ok: true, objects: 3, manifest: { schema_version: 12 },
    merge_plan: { home: '/demo/library', sessions_added: 2, sessions_replaced: 1, sessions_skipped: 0 } };
  input.value = 'first.tar.gz';
  const first = restore.onclick();
  assert.equal(restore.disabled, true);
  const n = f.requests.length;
  await restore.onclick();
  assert.equal(f.requests.length, n);
  f.requests.at(-1).resolve(report); await first;
  assert.equal(restore.textContent, '确认合并');
  assert.match(f.get('#bundle-report').textContent, /\/demo\/library/);
  assert.ok(!f.requests.some(r => r.command === 'bundle_restore'));
  input.value = 'second.tar.gz'; input.oninput();
  const changed = restore.onclick(); f.requests.at(-1).resolve(report); await changed;
  assert.ok(!f.requests.some(r => r.command === 'bundle_restore'));
  const confirm = restore.onclick(); f.requests.at(-1).resolve(report);
  await new Promise(setImmediate);
  assert.equal(f.requests.at(-1).command, 'bundle_restore');
  assert.equal(f.requests.at(-1).args.path, 'second.tar.gz');
  f.requests.at(-1).reject('对象恢复失败，数据库尚未写入'); await confirm;
  assert.equal(restore.disabled, false);
  assert.equal(restore.textContent, '合并恢复');
  assert.match(f.get('#bundle-report').textContent, /数据库尚未写入/);
});

test('failed bundle verification blocks restore and releases controls', async () => {
  const f = app(); f.bindBundleForTest(); f.get('#bundle-path').value = 'bad.tar.gz';
  const pending = f.get('#bundle-restore').onclick();
  f.requests.at(-1).resolve({ok:false, missing_referenced_objects:['missing']}); await pending;
  assert.ok(!f.requests.some(r => r.command === 'bundle_restore'));
  assert.match(f.get('#bundle-report').textContent, /缺少 1 个引用对象/);
  assert.equal(f.get('#bundle-verify').disabled, false);
});

test('search discloses the effective index scope', async () => {
  const f = app(); await f.renderSearch();
  f.requests.find(r => r.command === 'index_status').resolve({tool_index_full:false});
  await Promise.resolve();
  assert.match(f.get('#q-scope').textContent, /少于 3 字/);
  assert.match(f.get('#page-search').innerHTML, /20 万字符/);
});

test('backup picker cancels without changes and selected backup clears merge confirmation', async () => {
  const f = app(); f.bindBundleForTest();
  f.get('#bundle-path').value = 'old.tar.gz';
  const canceled = f.get('#bundle-path-pick').onclick();
  assert.equal(f.get('#bundle-restore').disabled, true);
  f.requests.at(-1).resolve(null); await canceled;
  assert.equal(f.get('#bundle-path').value, 'old.tar.gz');
  const preview = f.get('#bundle-restore').onclick();
  const report = { ok: true, manifest: {}, merge_plan: { home: '/test', sessions_added: 1, sessions_replaced: 0, sessions_skipped: 0 } };
  f.requests.at(-1).resolve(report); await preview;
  assert.equal(f.get('#bundle-restore').textContent, '确认合并');
  const selected = f.get('#bundle-path-pick').onclick();
  f.requests.at(-1).resolve('new.tar.gz'); await selected;
  assert.equal(f.get('#bundle-restore').textContent, '合并恢复');
  assert.equal(f.get('#bundle-path').value, 'new.tar.gz');
  const again = f.get('#bundle-restore').onclick();
  f.requests.at(-1).resolve(report); await again;
  assert.ok(!f.requests.some(r => r.command === 'bundle_restore'));
  const out = f.get('#bundle-out-pick').onclick();
  assert.equal(f.requests.at(-1).args.save, true);
  f.requests.at(-1).resolve('output.tar.gz'); await out;
  assert.equal(f.get('#bundle-out').value, 'output.tar.gz');
  assert.ok(!f.requests.some(r => r.command === 'bundle_create'));
});

test('manual import keeps the current page visible until one completed refresh', async () => {
  const f = app();
  f.get('.nav.active').dataset.page = 'sessions';
  f.get('#page-sessions').innerHTML = 'stable content';
  const button = f.get('#btn-import'), label = f.get('#btn-import span'), classes = new Set();
  button.classList = { add: x => classes.add(x), remove: x => classes.delete(x) };
  const pending = button.onclick();
  const importing = f.requests.at(-1);
  assert.equal(importing.command, 'import_now');
  assert.equal(f.get('#page-sessions').innerHTML, 'stable content');
  assert.equal(button.disabled, true);
  assert.equal(label.textContent, '采集中…');
  assert.ok(classes.has('busy'));
  importing.resolve({ messages_added: 2, lines_archived: 3 });
  await new Promise(setImmediate);
  const refresh = f.requests.at(-1);
  assert.equal(refresh.command, 'sessions');
  assert.equal(f.get('#page-sessions').innerHTML, 'stable content');
  refresh.resolve({ sessions: [] });
  await pending;
  assert.equal(button.disabled, false);
  assert.equal(label.textContent, '采集新对话');
  assert.ok(!classes.has('busy'));
  assert.doesNotMatch(f.get('#page-sessions').innerHTML, /加载中/);
});

test('storage copy distinguishes core data from backups and exports', () => {
  const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
  assert.match(source, /存储与备份/);
  assert.match(source, /会话原文归档/);
  assert.match(source, /备份与导出/);
  assert.match(source, /两者不是两份重复备份/);
  assert.doesNotMatch(source, /备份对象（objects）/);
});
