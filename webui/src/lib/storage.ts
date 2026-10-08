/** Copy before removing: a failed write must not lose the old preference. */
export function migratedValue(storage: Pick<Storage, "getItem" | "setItem" | "removeItem">, key: string, oldKey: string): string | null {
  const current = storage.getItem(key);
  const old = storage.getItem(oldKey);
  if (old !== null) {
    try {
      if (current === null) storage.setItem(key, old);
      storage.removeItem(oldKey);
    } catch { /* Keep the old value when storage is blocked or full. */ }
  }
  return current ?? old;
}
