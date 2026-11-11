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
    // Revocation runs once a released URL's fetch settles; the test waits for the twelfth one
    // instead of guessing how many ticks that takes.
    let twelveRevoked!: () => void;
    const evicted = new Promise<void>((r) => (twelveRevoked = r));
    vi.stubGlobal("URL", {
      createObjectURL: () => `blob:${next++}`,
      revokeObjectURL: (u: string) => {
        revoked.push(u);
        if (revoked.length === 12) twelveRevoked();
      },
    });
    const held = Array.from({ length: 140 }, (_, i) => acquireAttachmentUrl("s", `a${i}`));
    await Promise.all(held.map((h) => h.loading));
    expect(revoked).toEqual([]);

    for (const h of held.slice(0, 20)) h.release();
    await evicted;
    expect(revoked).toEqual(Array.from({ length: 12 }, (_, i) => `blob:${i}`));
    expect(await held[139]!.loading).toBe("blob:139");

    // A second user of the same picture shares its URL and keeps it alive.
    const again = acquireAttachmentUrl("s", "a139");
    held[139]!.release();
    expect(await again.loading).toBe("blob:139");
    again.release();
    again.release();
  });
});
