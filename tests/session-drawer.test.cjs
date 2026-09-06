const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function fixture() {
  const elements = new Map();
  const element = () => {
    let html = '';
    return { get innerHTML() { return html; }, set innerHTML(value) {
      html = value;
      for (const match of value.matchAll(/id="([^"]+)"/g)) elements.set(match[1], element());
    }, textContent: '', dataset: {},
    classList: { add() {}, remove() {} }, scrollIntoView() {} };
  };
  const get = (id) => elements.get(id) || null;
  const content = element();
  let html = '';
  Object.defineProperty(content, 'innerHTML', {
    get: () => html,
    set(value) {
      html = value;
      for (const id of [...elements.keys()]) if (!id.startsWith('drawer')) elements.delete(id);
      for (const match of value.matchAll(/id="([^"]+)"/g)) elements.set(match[1], element());
    },
  });
  elements.set('drawer-content', content);
  elements.set('drawer', element());
  const requests = [];
  const notices = [];
  const context = { document: { getElementById: get, querySelector: () => null, querySelectorAll: () => [] }, setTimeout };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/session-drawer.js'), 'utf8').replace('export function', 'function'), context);
  const drawer = context.createSessionDrawer({
    invoke: (command, args) => new Promise((resolve, reject) => requests.push({ command, args, resolve, reject })),
    $: (selector) => get(selector.slice(1)), esc: String, fmtTime: String,
    toast: (message) => notices.push(message), snapScrollboxTables() {}, bindCopyButtons() {},
    lineageGraphHtml: () => '', afterGraphRender() {}, armButton: (button, label, armed, fn) => { button.onclick = fn; },
  });
  return { drawer, requests, get, content, notices };
}
const data = (id) => ({ session: { project: id, agent: 'claude', resume_command: `resume ${id}` }, messages: [] });
const proof = (id) => ({ lines: 1, distinct_objects: 1, objects_bytes: 1, exclusive_objects: 1, file_path: id, source_exists: true });
const tick = () => new Promise(setImmediate);

test('latest session response owns the drawer and resume command', async () => {
  const f = fixture();
  const a = f.drawer.showSession('A'), b = f.drawer.showSession('B');
  f.requests[1].resolve(data('B')); await b;
  f.requests[0].resolve(data('A')); await a;
  assert.equal(f.drawer.resume, 'resume B');
  assert.equal(f.requests.filter(r => r.command === 'session_proof').length, 1);
});

test('late proof success or failure cannot overwrite the next session', async () => {
  for (const reject of [false, true]) {
    const f = fixture();
    const a = f.drawer.showSession('A'); f.requests[0].resolve(data('A')); await a;
    const oldProof = f.requests[1];
    const b = f.drawer.showSession('B'); f.requests[2].resolve(data('B')); await b;
    f.requests[3].resolve(proof('B.jsonl')); await tick();
    if (reject) oldProof.reject('old failure'); else oldProof.resolve(proof('A.jsonl'));
    await tick();
    assert.match(f.get('proof-body').innerHTML, /B.jsonl/);
    assert.equal(f.get('proof-body').textContent, '');
    const action = f.get('proof-export').onclick();
    assert.equal(f.requests.at(-1).args.sessionId, 'B');
    f.requests.at(-1).resolve({ lines: 1, path: 'B.jsonl' }); await action;
  }
});

test('close and another drawer invalidate pending session reads', async () => {
  for (const close of [true, false]) {
    const f = fixture();
    const pending = f.drawer.showSession('A');
    if (close) f.drawer.close(); else f.drawer.open('project details');
    f.requests[0].resolve(data('A')); await pending;
    assert.equal(f.content.innerHTML, close ? '' : 'project details');
    assert.equal(f.drawer.resume, '');
  }
});

test('obsolete writeback preview cannot bind a confirmation in another drawer', async () => {
  const f = fixture();
  const pending = f.drawer.showSession('A'); f.requests[0].resolve(data('A')); await pending;
  f.requests[1].resolve({ ...proof('A'), writeback_supported: true }); await tick();
  const preview = f.get('proof-wb').onclick();
  f.drawer.open('project details');
  f.requests[2].resolve({ target_exists: true, will_write: ['A'], vault_lines: 1 }); await preview;
  assert.equal(f.content.innerHTML, 'project details');
  assert.equal(f.get('wb-confirm'), null);
});

test('precompact toggle preserves its session and backend arguments', async () => {
  const f = fixture();
  const pending = f.drawer.showSession('A', 17);
  const d = data('A'); d.session.compact_line_no = 20;
  f.requests[0].resolve(d); await pending;
  assert.equal(f.requests[0].args.line, 17);
  f.get('btn-precompact').onclick();
  assert.equal(f.requests.at(-1).args.sessionId, 'A');
  assert.equal(f.requests.at(-1).args.beforeCompact, true);
  assert.equal(f.requests.at(-1).args.line, null);
});

test('search focus preserves exact message id and paging fetches the adjacent window', async () => {
  const f = fixture();
  const open = f.drawer.showSession('A', 100, false, null, 201);
  assert.equal(f.requests[0].command, 'session_window');
  assert.equal(f.requests[0].args.messageId, 201);
  f.requests[0].resolve({...data('A'), window_offset:125, total_messages:400, has_before:true, has_after:true,
    messages:[{message_id:201, line_no:100,kind:'user', content:'match',truncated:true}]});
  await open;
  assert.match(f.content.innerHTML,/展开原文/);
  const next = f.get('context-after').onclick();
  assert.equal(f.requests.at(-1).args.offset,275);
  assert.equal(f.requests.at(-1).args.messageId,201);
  f.drawer.close();
  f.requests.at(-1).resolve({...data('A'),window_offset:275,messages:[]});
  const html=f.content.innerHTML; await next;
  assert.equal(f.content.innerHTML,html);
});

test('ordinary tail view offers access to earlier archived messages', async () => {
  const f=fixture();
  const open=f.drawer.showSession('A');
  f.requests[0].resolve({...data('A'),total_messages:900}); await open;
  f.get('context-browse').onclick();
  assert.equal(f.requests.at(-1).command,'session_window');
  assert.equal(f.requests.at(-1).args.offset,750);
});
