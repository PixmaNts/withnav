/**
 * The transcript's resolved image URLs, least recently used first out. A `data:` URL of a full-size
 * attachment weighs up to about 5.2 M characters, a conversation view lives as long as its tab, and
 * up to 12 tabs stay open, so an unbounded map kept every picture a long conversation ever showed in
 * memory, once per open tab. Eviction only forgets the URL: a thumbnail already drawn keeps its
 * `src`, and one drawn later asks the extension host again.
 *
 * `false` records an attachment known to be unavailable; it costs no characters.
 */
export class ImageCache {
  private readonly entries = new Map<string, string | false>();
  private chars = 0;

  constructor(readonly maxEntries: number, readonly maxChars: number) {}

  get size(): number { return this.entries.size; }
  get totalChars(): number { return this.chars; }

  /** The cached value, marked as most recently used; `undefined` when not cached. */
  get(id: string): string | false | undefined {
    const value = this.entries.get(id);
    if (value === undefined) return undefined;
    this.entries.delete(id);
    this.entries.set(id, value);
    return value;
  }

  /** Stores `value` as most recently used. A URL longer than the whole budget is not kept. */
  set(id: string, value: string | false): void {
    this.delete(id);
    if (typeof value === "string" && value.length > this.maxChars) return;
    this.entries.set(id, value);
    if (typeof value === "string") this.chars += value.length;
    for (const [oldest, old] of this.entries) {
      if (this.entries.size <= this.maxEntries && this.chars <= this.maxChars) break;
      this.entries.delete(oldest);
      if (typeof old === "string") this.chars -= old.length;
    }
  }

  delete(id: string): void {
    const old = this.entries.get(id);
    if (old === undefined) return;
    this.entries.delete(id);
    if (typeof old === "string") this.chars -= old.length;
  }
}
