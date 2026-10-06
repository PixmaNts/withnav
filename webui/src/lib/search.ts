/** Same ranking as the VS Code pickers: exact name, then prefix, then substring, then description only. */
export function ranked<T>(items: T[], query: string, name: (item: T) => string, description: (item: T) => string): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return items;
  const rank = (item: T): number => {
    const n = name(item).toLowerCase();
    return n === q ? 0 : n.startsWith(q) ? 1 : n.includes(q) ? 2 : 3;
  };
  return items
    .filter((item) => `${name(item)} ${description(item)}`.toLowerCase().includes(q))
    .map((item, index) => ({ item, index, rank: rank(item) }))
    .sort((a, b) => a.rank - b.rank || a.index - b.index)
    .map((x) => x.item);
}
