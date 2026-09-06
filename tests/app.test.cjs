const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function app() {
  const nodes = new Map(), lists = new Map(), requests = [], notices = [];
  const make = () => ({ innerHTML: '', textContent: '', value: '', dataset: {}, style: {},
    focus() {}, remove() {}, addEventListener() {}, classList: { add() {}, remove() {} }, querySelectorAll: () => [] });
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
  vm.runInContext(ui('app.js').replace(/^import .*;\r?\n/gm, '') + '\nglobalThis.testApp = { renderSearch, renderMemory, armButton, memoryGraphHtml, lineageGraphHtml, bindMemoryGraph, setMemoryView: v => { memView = v; } };', context);
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
