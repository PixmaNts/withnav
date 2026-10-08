import { describe, expect, it } from "vitest";
import { formatRoute, parseRoute } from "./router";

describe("studio route", () => {
  it("parses ws, flow, loc and mock", () => {
    const r = parseRoute("#/studio?ws=%2Frepo&flow=guard-mutations&loc=workspace&mock=1");
    expect(r).toMatchObject({ page: "studio", workspace: "/repo", flow: "guard-mutations", loc: "workspace", mock: true, session: null });
  });

  it("formats back to the same address and keeps chat routes unchanged", () => {
    const hash = "#/studio?ws=%2Frepo&flow=guard-mutations&loc=global";
    expect(formatRoute(parseRoute(hash))).toBe(hash);
    expect(formatRoute({ ...parseRoute(hash), page: "chat", session: "s1" })).toBe("#/chat?ws=%2Frepo&s=s1");
    expect(parseRoute("#/chat?ws=%2Frepo&s=s1&flow=x").flow).toBeNull();
  });
});
