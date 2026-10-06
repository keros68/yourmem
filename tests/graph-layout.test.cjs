const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const context = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/graph-layout.js'), 'utf8').replaceAll('export function', 'function'), context);

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

test('tree slots keep children under their parent and never overlap within a layer', () => {
  const ids = ['R1', 'R2', 'a', 'b', 'c', 'd', 'x'];
  const edges = [['R1','a'], ['R2','b'], ['R1','c'], ['R2','d'], ['a','x'], ['b','x']];
  const { depth } = context.graphDepths(ids, edges);
  const { slot, width } = context.treeSlots(ids, edges, depth, (p, q) => p.localeCompare(q));
  assert.equal(width, 4);
  assert.deepEqual(['a','c'].map(id => slot.get(id)), [0, 1]);
  assert.equal(slot.get('R1'), 0.5);
  assert.equal(slot.get('x'), 0);
  for (const d of new Set(depth.values())) {
    const s = ids.filter(id => depth.get(id) === d).map(id => slot.get(id)).sort((p, q) => p - q);
    for (let i = 1; i < s.length; i++) assert.ok(s[i] - s[i - 1] >= 1);
  }
});
