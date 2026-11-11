import { describe, expect, it, vi } from "vitest";

vi.mock("../api/client", () => ({ api: { upload: vi.fn() } }));
import { uploadImages } from "./uploads";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** An upload whose every call waits until the test settles it. */
function controlledUpload() {
  const calls: { file: File; done: ReturnType<typeof deferred<{ attachment_id: string }>> }[] = [];
  const waiting: { count: number; resolve: () => void }[] = [];
  let running = 0;
  let peak = 0;
  let finished = 0;
  const upload = vi.fn((_session: string, file: File) => {
    const done = deferred<{ attachment_id: string }>();
    calls.push({ file, done });
    for (const w of waiting) if (calls.length >= w.count) w.resolve();
    running += 1;
    peak = Math.max(peak, running);
    return done.promise.finally(() => {
      running -= 1;
      finished += 1;
    });
  });
  /** Settles once `count` uploads have started. */
  const started = (count: number) => new Promise<void>((resolve) => (calls.length >= count ? resolve() : waiting.push({ count, resolve })));
  return { upload, calls, started, peak: () => peak, finished: () => finished };
}

const files = (n: number) => Array.from({ length: n }, (_, i) => new File(["x"], `f${i}.png`));

describe("image uploads", () => {
  // Given five images and a limit of three
  // Then three uploads start before any finishes, never more than three run, and the ids follow the files
  it("runs uploads in parallel up to the limit and keeps the files' order", async () => {
    const { upload, calls, started, peak } = controlledUpload();
    const picked = files(5);
    const result = uploadImages("s1", picked, upload, 3);
    expect(calls.map((c) => c.file)).toEqual(picked.slice(0, 3));

    // Finish out of order: the third, then the first; each frees a slot for the next file.
    calls[2]!.done.resolve({ attachment_id: "id2" });
    await started(4);
    calls[0]!.done.resolve({ attachment_id: "id0" });
    await started(5);
    expect(calls.map((c) => c.file)).toEqual(picked);
    calls[4]!.done.resolve({ attachment_id: "id4" });
    calls[1]!.done.resolve({ attachment_id: "id1" });
    calls[3]!.done.resolve({ attachment_id: "id3" });

    expect(await result).toEqual(["id0", "id1", "id2", "id3", "id4"]);
    expect(peak()).toBe(3);
  });

  // Given the same picture twice in one message
  // Then it is stored once and both blocks carry its id
  it("uploads the same file once", async () => {
    const upload = vi.fn(async (_s: string, file: File) => ({ attachment_id: `id-${file.name}` }));
    const [a, b] = files(2);
    expect(await uploadImages("s2", [a!, b!, a!], upload, 3)).toEqual(["id-f0.png", "id-f1.png", "id-f0.png"]);
    expect(upload).toHaveBeenCalledTimes(2);
  });

  // Given an upload failing while others are in flight and more are queued
  // Then nothing new starts, the in-flight uploads are awaited, the error is thrown,
  // and the retry reuses what was stored
  it("stops on a failure and lets the retry reuse finished uploads", async () => {
    const { upload, calls, finished } = controlledUpload();
    const picked = files(5);
    let finishedWhenSettled = -1;
    const result = uploadImages("s3", picked, upload, 3).finally(() => (finishedWhenSettled = finished()));
    expect(calls).toHaveLength(3);
    calls[0]!.done.reject(new Error("quota"));
    calls[1]!.done.resolve({ attachment_id: "id1" });
    calls[2]!.done.resolve({ attachment_id: "id2" });
    // Had a queued upload started after the failure, it would never settle and neither would this.
    await expect(result).rejects.toThrow("quota");
    expect(finishedWhenSettled).toBe(3);
    expect(calls).toHaveLength(3);

    const retry = vi.fn(async (_s: string, file: File) => ({ attachment_id: `new-${file.name}` }));
    expect(await uploadImages("s3", picked, retry, 3)).toEqual(["new-f0.png", "id1", "id2", "new-f3.png", "new-f4.png"]);
    expect(retry.mock.calls.map(([, file]) => file)).toEqual([picked[0], picked[3], picked[4]]);

    // Another session stores its own copy.
    const other = vi.fn(async () => ({ attachment_id: "x" }));
    await uploadImages("s4", [picked[1]!], other, 3);
    expect(other).toHaveBeenCalledTimes(1);
  });
});
