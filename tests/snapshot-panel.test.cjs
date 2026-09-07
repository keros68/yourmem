const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

function fixture() {
  const nodes = new Map();
  const requests = [];
  const make = () => ({
    innerHTML: "", textContent: "", value: "", disabled: false, dataset: {},
    classList: { add() {}, remove() {}, toggle() {} },
    querySelectorAll: () => [], scrollIntoView() {}, dispatchEvent() {},
  });
  const get = (selector) => {
    if (!nodes.has(selector)) nodes.set(selector, make());
    return nodes.get(selector);
  };
  const root = { querySelector: get };
  const context = {
    globalThis: {},
    Event: class Event { constructor(type) { this.type = type; } },
  };
  context.globalThis.Event = context.Event;
  vm.createContext(context);
  const source = fs.readFileSync("ui/snapshot-panel.js", "utf8")
    .replace(/^export function /gm, "function ")
    + "\nthis.testSnapshot = { snapshotPanelHtml, bindSnapshotPanel };";
  vm.runInContext(source, context);
  const invoke = (command, args) => new Promise((resolve, reject) => requests.push({ command, args, resolve, reject }));
  const api = context.testSnapshot.bindSnapshotPanel({ invoke, root, bundleInput: get("#bundle-path") });
  return { ...api, get, requests, html: context.testSnapshot.snapshotPanelHtml() };
}

test("snapshot panel lists, exports into bundle input, and creates a snapshot", async () => {
  const f = fixture();
  assert.match(f.html, /日常增量快照/);
  const listed = f.requests.shift();
  assert.equal(listed.command, "snapshot_list");
  listed.resolve({ root: "/backup", policy: { keep_recent: 7, keep_monthly: 6 }, snapshots: [{ id: "s1", created_at: "2026-09-07T10:00:00Z", db_bytes: 1024, objects_bytes: 2048, objects: 2 }] });
  await new Promise(setImmediate);
  assert.match(f.get("#snapshot-list").innerHTML, /data-snapshot-export="s1"/);
  const button = f.get("#snapshot-create");
  const creating = button.onclick();
  const req = f.requests.at(-1);
  assert.equal(req.command, "snapshot_create");
  req.resolve({ id: "s2", new_objects: 3, new_bytes: 4096 });
  await new Promise(setImmediate);
  const refresh = f.requests.at(-1);
  assert.equal(refresh.command, "snapshot_list");
  refresh.resolve({ snapshots: [] });
  await creating;
  assert.match(f.get("#snapshot-report").textContent, /已创建快照/);
});

test("refresh stays busy while its list request is pending", async () => {
  const f = fixture();
  const first = f.requests.shift();
  f.get("#snapshot-refresh").onclick({ type: "click" });
  assert.equal(f.requests.length, 0);
  first.resolve({ snapshots: [] });
  await new Promise(setImmediate);
  f.get("#snapshot-refresh").onclick({ type: "click" });
  assert.equal(f.requests.at(-1).command, "snapshot_list");
});

test("cleanup requires a plan and invalidates confirmation when rules change", async () => {
  const f = fixture();
  f.requests.shift().resolve({ snapshots: [] });
  await new Promise(setImmediate);
  const planAction = f.get("#snapshot-cleanup-plan").onclick();
  const plan = f.requests.at(-1);
  assert.equal(plan.command, "snapshot_cleanup_plan");
  plan.resolve({ token: "t1", remove_count: 0, remove_ids: [], reclaim_bytes: 8192 });
  await planAction;
  assert.match(f.get("#snapshot-cleanup-report").textContent, /清理 8.0 KB 个未引用对象/);
  const cleanup = f.get("#snapshot-cleanup-confirm").onclick();
  assert.equal(f.requests.at(-1).command, "snapshot_cleanup");
  f.requests.at(-1).resolve({ removed: 0, reclaimed_bytes: 8192 });
  await new Promise(setImmediate);
  f.requests.at(-1).resolve({ snapshots: [] });
  await cleanup;
  f.get("#snapshot-keep-recent").value = "6";
  f.get("#snapshot-keep-recent").oninput();
  const before = f.requests.length;
  await f.get("#snapshot-cleanup-confirm").onclick();
  assert.equal(f.requests.length, before);
});
