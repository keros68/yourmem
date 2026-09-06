const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const context = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/graph-layout.js'), 'utf8').replace('export function', 'function'), context);

test('converging branches use the longest path regardless of input order', () => {
  for (const ids of [['A','B','C','D'], ['D','C','B','A']]) {
    const result = context.graphDepths(ids, [['A','B'], ['B','C'], ['D','C'], ['A','B']]);
    assert.equal(result.depth.get('C'), 2);
    assert.equal(result.depth.get('B'), 1);
    assert.equal(result.unresolved.length, 0);
  }
});
test('historical cycles terminate and are explicitly marked; dangling edges are ignored', () => {
  const result = context.graphDepths(['A','B','C','D'], [['A','B'], ['B','A'], ['B','C'], ['missing','D']]);
  assert.equal([...result.unresolved].sort().join(','), 'A,B,C');
  assert.equal(result.depth.get('D'), 0);
});
test('long chains do not recurse or stop at an arbitrary traversal guard', () => {
  const ids = Array.from({length: 12000}, (_, i) => String(i));
  const result = context.graphDepths(ids, ids.slice(1).map((id,i) => [ids[i], id]));
  assert.equal(result.depth.get('11999'), 11999);
});
