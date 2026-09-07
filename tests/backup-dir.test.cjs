const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

test('folder selection fills the input; only Save persists; cancel leaves it unchanged', async () => {
  const nodes = new Map();
  const $ = id => { if (!nodes.has(id)) nodes.set(id, { value: '', disabled: false, textContent: '', click() {} }); return nodes.get(id); };
  let selection = 'D:/chosen', calls = [];
  const invoke = async (name, args) => {
    calls.push({ name, args });
    if (name === 'backup_dir_pick') return selection;
    if (name === 'backup_dir_set') return { effective: args.path, moved_files: 3 };
    throw Error(name);
  };
  const source = fs.readFileSync(require('node:path').join(__dirname, '../ui/app.js'), 'utf8');
  const handlers = source.slice(source.indexOf('  let curBackupDir = bd.effective;'), source.indexOf('  $("#storage-compact").onclick'));
  vm.runInNewContext(handlers, { $, invoke, bd: { effective: 'D:/original' }, toast() {} });
  $('#bundle-out').value = 'D:/original/backup.tar.gz';
  await $('#backup-dir-pick').onclick();
  assert.equal($('#backup-dir').value, 'D:/chosen');
  assert.deepEqual(calls.map(c => c.name), ['backup_dir_pick']);
  assert.match($('#backup-dir-report').textContent, /保存/);
  selection = null;
  await $('#backup-dir-pick').onclick();
  assert.equal($('#backup-dir').value, 'D:/chosen');
  await $('#backup-dir-save').onclick();
  assert.equal(calls.at(-1).name, 'backup_dir_set');
  assert.equal(calls.at(-1).args.path, 'D:/chosen');
  assert.equal($('#backup-dir-save').disabled, false);
  assert.equal($('#bundle-out').value, 'D:/chosen/backup.tar.gz');
  assert.match($('#backup-dir-report').textContent, /已迁移 3 个文件/);
});
