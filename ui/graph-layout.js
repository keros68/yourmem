// 多前驱有向图按最长路径分层；无法拓扑排序的节点单独标记，旧数据有环也能显示。
export function graphDepths(ids, edges) {
  const depth = new Map(ids.map(id => [id, 0]));
  const incoming = new Map(ids.map(id => [id, 0]));
  const children = new Map(ids.map(id => [id, new Set()]));
  for (const [from, to] of edges) {
    if (!depth.has(from) || !depth.has(to) || children.get(from).has(to)) continue;
    children.get(from).add(to);
    incoming.set(to, incoming.get(to) + 1);
  }
  const queue = ids.filter(id => incoming.get(id) === 0).sort();
  for (let i = 0; i < queue.length; i++) {
    const from = queue[i];
    for (const to of children.get(from)) {
      depth.set(to, Math.max(depth.get(to), depth.get(from) + 1));
      incoming.set(to, incoming.get(to) - 1);
      if (incoming.get(to) === 0) queue.push(to);
    }
  }
  const unresolved = ids.filter(id => incoming.get(id) > 0);
  for (const id of unresolved) depth.set(id, 0);
  return { depth, unresolved };
}

// 树形排位：每个节点挂在深度恰好少一层的首个父节点下，叶子依次占位，父节点居中于子树，
// 同层节点间隔至少一个位。多父边仍照画，只是不参与排位。order 为同层兄弟的先后比较函数。
export function treeSlots(ids, edges, depth, order) {
  const primary = new Map();
  for (const [from, to] of edges) {
    if (primary.has(to) || !depth.has(from) || !depth.has(to)) continue;
    if (depth.get(from) === depth.get(to) - 1) primary.set(to, from);
  }
  const kids = new Map(ids.map(id => [id, []]));
  for (const [c, p] of primary) kids.get(p).push(c);
  for (const list of kids.values()) list.sort(order);
  const slot = new Map();
  let next = 0;
  const place = (id) => {
    const cs = kids.get(id);
    if (!cs.length) { slot.set(id, next++); return; }
    cs.forEach(place);
    slot.set(id, (slot.get(cs[0]) + slot.get(cs[cs.length - 1])) / 2);
  };
  ids.filter(id => !primary.has(id)).sort(order).forEach(place);
  return { slot, width: next };
}
