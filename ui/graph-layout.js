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
