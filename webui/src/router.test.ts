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

describe("memory and projects routes", () => {
  it("parses the open item and the needs view", () => {
    expect(parseRoute("#/memory?ws=%2Frepo&item=01ABC&view=needs")).toMatchObject({ page: "memory", workspace: "/repo", item: "01ABC", view: "needs", session: null });
    expect(parseRoute("#/projects?item=p1")).toMatchObject({ page: "projects", item: "p1", view: null });
    expect(parseRoute("#/memory")).toMatchObject({ page: "memory", item: null, view: null });
  });

  it("formats back to the same address", () => {
    for (const hash of ["#/memory", "#/memory?ws=%2Frepo&item=01ABC", "#/memory?view=needs", "#/projects?ws=%2Frepo&item=p1"]) {
      expect(formatRoute(parseRoute(hash))).toBe(hash);
    }
  });

  it("does not carry the open item to other pages", () => {
    const memory = parseRoute("#/memory?ws=%2Frepo&item=01ABC&view=needs");
    expect(formatRoute({ ...memory, page: "chat", session: "s1" })).toBe("#/chat?ws=%2Frepo&s=s1");
    expect(formatRoute({ ...memory, page: "settings", section: "layers" })).toBe("#/settings?ws=%2Frepo&section=layers");
    expect(formatRoute({ ...memory, page: "projects" })).toBe("#/projects?ws=%2Frepo&item=01ABC");
    expect(parseRoute("#/chat?item=x&view=needs")).toMatchObject({ item: null, view: null });
  });
});
