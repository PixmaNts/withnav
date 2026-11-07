import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/client", () => ({ api: { attachment: vi.fn(async () => new Blob(["x"])) } }));
import { acquireAttachmentUrl } from "./Images";

describe("attachment URLs", () => {
  afterEach(() => vi.unstubAllGlobals());

  // Given more images on screen than the cache bound
  // Then none of them is revoked while shown, and the released ones are revoked past the bound
  it("never revokes a URL still shown", async () => {
    let next = 0;
    const revoked: string[] = [];
    vi.stubGlobal("URL", { createObjectURL: () => `blob:${next++}`, revokeObjectURL: (u: string) => revoked.push(u) });
    const held = Array.from({ length: 140 }, (_, i) => acquireAttachmentUrl("s", `a${i}`));
    await Promise.all(held.map((h) => h.loading));
    await Promise.resolve();
    expect(revoked).toEqual([]);

    for (const h of held.slice(0, 20)) h.release();
    await new Promise((r) => setTimeout(r, 0));
    expect(revoked).toHaveLength(12);
    expect(await held[139]!.loading).toBe("blob:139");

    // A second user of the same picture shares its URL and keeps it alive.
    const again = acquireAttachmentUrl("s", "a139");
    held[139]!.release();
    expect(await again.loading).toBe("blob:139");
    again.release();
    again.release();
  });
});
