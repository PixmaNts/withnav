export function ranked<T>(items: T[], query: string, name: (item: T) => string, description: (item: T) => string): T[] {
  const q = query.trim().toLowerCase();
  const rank = (item: T): number => {
    const n = name(item).toLowerCase();
    return n === q ? 0 : n.startsWith(q) ? 1 : n.includes(q) ? 2 : 3;
  };
  return items.filter(item => !q || `${name(item)} ${description(item)}`.toLowerCase().includes(q))
    .sort((a, b) => rank(a) - rank(b));
}
export const PICKER_LIMIT = 50;
