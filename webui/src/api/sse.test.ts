import { describe, expect, it } from "vitest";
import { nextBackoff, SseDecoder } from "./sse";

describe("SseDecoder", () => {
  it("tells resync control frames from data frames and skips keep-alives", () => {
    const frames = new SseDecoder().push(": keep-alive\n\nevent: resync\ndata: cursor out of range; reload\n\nid: 3\ndata: {}\n\n");
    expect(frames).toEqual([
      { event: "resync", data: "cursor out of range; reload", id: undefined },
      { event: "message", data: "{}", id: "3" },
    ]);
  });

  it("joins a frame split across chunks at any byte", () => {
    const raw = 'id: 7\ndata: {"seq":7,"kind":"llm.delta"}\n\nid: 8\ndata: {"seq":8}\n\n';
    for (let cut = 1; cut < raw.length; cut++) {
      const decoder = new SseDecoder();
      const frames = [...decoder.push(raw.slice(0, cut)), ...decoder.push(raw.slice(cut))];
      expect(frames.map((f) => f.id)).toEqual(["7", "8"]);
      expect(frames[0]!.data).toBe('{"seq":7,"kind":"llm.delta"}');
    }
  });

  it("accepts CRLF and CR line ends, even a CRLF split between chunks", () => {
    const decoder = new SseDecoder();
    expect(decoder.push("id: 1\r\ndata: a\r")).toEqual([]);
    expect(decoder.push("\n\r\nid: 2\rdata: b\r\r")).toEqual([{ event: "message", data: "a", id: "1" }]);
    // The final `\r` may still turn out to be half of a CRLF: the next chunk settles it.
    expect(decoder.push("id: 3\n")).toEqual([{ event: "message", data: "b", id: "2" }]);
  });

  it("joins multi-line data with newlines and keeps the last id", () => {
    const frames = new SseDecoder().push("id: 4\ndata: one\ndata:two\n\n\n\n");
    expect(frames).toEqual([{ event: "message", data: "one\ntwo", id: "4" }]);
  });

  it("emits nothing for a frame without data", () => {
    expect(new SseDecoder().push("event: ping\n\nid: 9\n\n")).toEqual([]);
  });
});

describe("reconnect backoff", () => {
  it("doubles up to five seconds", () => {
    const seen = [250];
    for (let i = 0; i < 8; i++) seen.push(nextBackoff(seen[seen.length - 1]!));
    expect(seen.slice(0, 5)).toEqual([250, 500, 1000, 2000, 4000]);
    expect(seen[seen.length - 1]).toBe(5000);
  });
});
