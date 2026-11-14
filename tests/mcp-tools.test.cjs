const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const tick = () => new Promise(setImmediate);

function fixture() {
  const nodes = new Map();
  const requests = [];
  const get = (selector) => {
    if (!nodes.has(selector)) nodes.set(selector, { innerHTML: "", textContent: "" });
    return nodes.get(selector);
  };
  const context = { Date };
  vm.createContext(context);
  const source = fs.readFileSync(path.join(__dirname, "../ui/mcp-tools.js"), "utf8")
    .replace(/^export function /gm, "function ")
    + "\nthis.testTools = { mcpToolsHtml, bindMcpTools };";
  vm.runInContext(source, context);
  const invoke = (command, args) => new Promise((resolve, reject) => requests.push({ command, args, resolve, reject }));
  const bind = () => context.testTools.bindMcpTools({ invoke, root: { querySelector: get } });
  bind();
  // 设置页重画：容器换成新节点，再绑定一次
  const rerender = () => { nodes.clear(); bind(); };
  const toggle = (name) => get("#mcp-tools-list").onchange({ target: { dataset: { mcpTool: name } } });
  const commands = () => requests.map((r) => r.command);
  return { get, requests, rerender, toggle, commands, html: context.testTools.mcpToolsHtml() };
}

const daysAgo = (n, now = Date.now()) => new Date(now - n * 86400000).toISOString();
const inventory = ({ artifactsOff = false, searchOff = false, bytesDaysAgo = 5, now = Date.now() } = {}) => ({
  days: 30,
  recorded_since: daysAgo(5, now),
  bytes_recorded_since: bytesDaysAgo == null ? null : daysAgo(bytesDaysAgo, now),
  tools: [
    { name: "search_history", definition_chars: 900, calls: 85, response_bytes: 4096, disabled: searchOff, in_instructions: true },
    { name: "list_artifacts", definition_chars: 400, calls: 0, response_bytes: 0, disabled: artifactsOff, in_instructions: false },
  ],
});

async function loaded(data) {
  const f = fixture();
  await tick();
  f.requests.shift().resolve(data);
  await tick();
  return f;
}

const OFF = (name) => new RegExp(`data-mcp-tool="${name}"\\s+\\/>[\\s\\S]*?已停用`);
const ON = (name) => new RegExp(`data-mcp-tool="${name}" checked\\s+\\/>`);

test("lists every tool with usage and totals", async () => {
  const f = fixture();
  assert.match(f.html, /提供给 agent 的工具/);
  await tick();
  const listed = f.requests.shift();
  assert.equal(listed.command, "mcp_tools");
  listed.resolve(inventory({ artifactsOff: true }));
  await tick();
  const table = f.get("#mcp-tools-list").innerHTML;
  assert.match(table, /近 30 天调用/);
  assert.match(table, /search_history[\s\S]*搜索历史对话 · 接入指令提到[\s\S]*85[\s\S]*4\.0 KB/);
  assert.match(table, ON("search_history"));
  assert.match(table, OFF("list_artifacts"));
  assert.match(f.get("#mcp-tools-summary").textContent, /^工具说明合计 1300 字符，其中已停用 400 字符。调用记录始于 \d{4}-\d{2}-\d{2}。$/);
});

test("says when response sizes started being recorded later than calls", async () => {
  const later = await loaded(inventory({ bytesDaysAgo: 1 }));
  assert.match(later.get("#mcp-tools-summary").textContent, /调用记录始于 \d{4}-\d{2}-\d{2}。返回量自 \d{4}-\d{2}-\d{2} 起记录。$/);
  // 同一天内先有未记返回量的调用：起点不同就注明，即使日期相同
  const sameDay = await loaded({ ...inventory(), bytes_recorded_since: daysAgo(5, Date.now() + 60000) });
  assert.match(sameDay.get("#mcp-tools-summary").textContent, /调用记录始于 (\d{4}-\d{2}-\d{2})。返回量自 \d{4}-\d{2}-\d{2} 起记录。$/);
  const never = await loaded(inventory({ bytesDaysAgo: null }));
  assert.match(never.get("#mcp-tools-summary").textContent, /返回量暂无记录。$/);
});

test("unchecking a tool disables it, locks the switches while saving, then reloads", async () => {
  const f = await loaded(inventory());
  const change = f.toggle("search_history");
  const set = f.requests.shift();
  assert.equal(set.command, "mcp_tool_set_disabled");
  assert.deepEqual({ ...set.args }, { name: "search_history", disabled: true });
  assert.match(f.get("#mcp-tools-list").innerHTML, /data-mcp-tool="list_artifacts" checked disabled/);
  set.resolve({ ok: true });
  await tick();
  const reload = f.requests.shift();
  assert.equal(reload.command, "mcp_tools");
  reload.resolve(inventory({ searchOff: true }));
  await change;
  assert.match(f.get("#mcp-tools-report").textContent, /^已停用 search_history，对新开的对话生效。接入指令提到了这个工具/);
  const table = f.get("#mcp-tools-list").innerHTML;
  assert.match(table, OFF("search_history"));
  assert.match(table, ON("list_artifacts"));
});

test("a failed change is reported and the switch goes back, even when the reload also fails", async () => {
  const f = await loaded(inventory({ artifactsOff: true }));
  const change = f.toggle("list_artifacts");
  f.requests.shift().reject("config.json 无法解析");
  await tick();
  f.requests.shift().reject("资料库暂时打不开");
  await change;
  assert.equal(f.get("#mcp-tools-report").textContent, "操作失败：config.json 无法解析 列表读取失败：资料库暂时打不开");
  assert.match(f.get("#mcp-tools-list").innerHTML, OFF("list_artifacts"));
});

test("a saved change stays shown with its message when only the reload fails", async () => {
  const f = await loaded(inventory());
  const change = f.toggle("search_history");
  f.requests.shift().resolve({ ok: true });
  await tick();
  f.requests.shift().reject("资料库暂时打不开");
  await change;
  assert.match(f.get("#mcp-tools-report").textContent, /^已停用 search_history，对新开的对话生效。接入指令提到了这个工具.* 列表读取失败：资料库暂时打不开$/);
  assert.match(f.get("#mcp-tools-list").innerHTML, OFF("search_history"));
});

test("a second change is ignored until the first one has been saved and reloaded", async () => {
  const f = await loaded(inventory());
  const first = f.toggle("search_history");
  await f.toggle("list_artifacts");
  assert.deepEqual(f.commands(), ["mcp_tool_set_disabled"]);
  f.requests.shift().resolve({ ok: true });
  await tick();
  f.requests.shift().resolve(inventory({ searchOff: true }));
  await first;
  assert.deepEqual(f.commands(), []);
  assert.match(f.get("#mcp-tools-list").innerHTML, ON("list_artifacts"));
});

test("a read from before the page was redrawn does not touch the new page", async () => {
  const f = fixture();
  await tick();
  const stale = f.requests.shift();
  f.rerender();
  await tick();
  f.requests.shift().resolve(inventory({ artifactsOff: true }));
  await tick();
  stale.resolve(inventory());
  await tick();
  assert.match(f.get("#mcp-tools-list").innerHTML, OFF("list_artifacts"));
});

test("a page redrawn during a save reads the list only after the save has finished", async () => {
  const f = await loaded(inventory());
  f.toggle("search_history");
  f.rerender();
  await tick();
  assert.deepEqual(f.commands(), ["mcp_tool_set_disabled"]);
  f.requests.shift().resolve({ ok: true });
  await tick();
  assert.deepEqual(f.commands(), ["mcp_tools", "mcp_tools"]);
  for (const read of f.requests.splice(0)) read.resolve(inventory({ searchOff: true }));
  await tick();
  assert.match(f.get("#mcp-tools-list").innerHTML, OFF("search_history"));
  assert.equal(f.get("#mcp-tools-report").textContent, "");
});

test("a tool without a label shows only the instruction note", async () => {
  const f = await loaded({ days: 30, recorded_since: null, bytes_recorded_since: null, tools: [
    { name: "brand_new_tool", definition_chars: 10, calls: 0, response_bytes: 0, disabled: false, in_instructions: true },
  ] });
  assert.match(f.get("#mcp-tools-list").innerHTML, /brand_new_tool <span[^>]*>接入指令提到<\/span>/);
  assert.equal(f.get("#mcp-tools-summary").textContent, "工具说明合计 10 字符。暂无调用记录。");
});
