// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { authHeaders, onUnauthorized, pairFromHash, token } from "./http";

describe("pairing and token acquisition", () => {
  beforeEach(() => {
    sessionStorage.clear();
    window.location.hash = "";
    vi.restoreAllMocks();
  });

  it("reads #pair=<code>, strips the fragment immediately, and exchanges it for a credential", async () => {
    window.location.hash = "#pair=test-pair-code-123";
    const replaceStateSpy = vi.spyOn(window.history, "replaceState");

    let exchangeBody: unknown;
    let urlCalled = "";
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        urlCalled = url;
        exchangeBody = JSON.parse((init?.body as string) || "{}");
        return new Response(
          JSON.stringify({
            credential: "mock-credential-abcdef",
            expires_at: "2026-11-14T20:00:00Z",
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }),
    );

    await pairFromHash();

    // Fragment stripped immediately
    expect(replaceStateSpy).toHaveBeenCalledWith(
      null,
      "",
      window.location.pathname + window.location.search,
    );
    // Code posted to exchange endpoint
    expect(urlCalled).toBe("/v1/pair/exchange");
    expect(exchangeBody).toEqual({ code: "test-pair-code-123" });
    // Returned credential stored in sessionStorage and available via token()
    expect(token()).toBe("mock-credential-abcdef");
    expect(authHeaders().get("authorization")).toBe("Bearer mock-credential-abcdef");
  });

  it("ignores legacy #token= fragment and does not adopt it", async () => {
    window.location.hash = "#token=legacy-token-not-accepted";
    const replaceStateSpy = vi.spyOn(window.history, "replaceState");
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);

    await pairFromHash();

    expect(replaceStateSpy).not.toHaveBeenCalled();
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(token()).toBeNull();
  });

  it("does nothing when URL has no hash", async () => {
    window.location.hash = "";
    const replaceStateSpy = vi.spyOn(window.history, "replaceState");
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);

    await pairFromHash();

    expect(replaceStateSpy).not.toHaveBeenCalled();
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(token()).toBeNull();
  });

  it("reports unauthorized when exchange returns 403", async () => {
    window.location.hash = "#pair=expired-or-used-code";
    const onUnauthorizedListener = vi.fn();
    const unsubscribe = onUnauthorized(onUnauthorizedListener);

    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ error: "invalid or expired pairing code" }), {
            status: 403,
            headers: { "content-type": "application/json" },
          }),
      ),
    );

    await pairFromHash();

    expect(onUnauthorizedListener).toHaveBeenCalled();
    expect(token()).toBeNull();
    unsubscribe();
  });
});
