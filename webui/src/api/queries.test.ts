import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { keepMcpStatus, keys, newerMcp } from "./queries";
import type { McpStatus } from "./types";

const status = (incarnation: string, revision: number, state = "ready"): McpStatus => ({
  incarnation,
  revision,
  servers: [
    {
      server_id: "github",
      transport: "stdio",
      state,
      tool_count: state === "ready" ? 12 : 0,
      skipped_tools: [],
      skipped_more: 0,
      error: null,
      stderr_tail: "",
      stderr_truncated: false,
    },
  ],
});

describe("MCP status ordering", () => {
  it("keeps the higher revision of one incarnation and takes any other incarnation", () => {
    expect(newerMcp(undefined, status("a", 1))).toBe(true);
    expect(newerMcp(status("a", 3), status("a", 2))).toBe(false);
    expect(newerMcp(status("a", 3), status("a", 3))).toBe(true);
    expect(newerMcp(status("a", 3), status("a", 4))).toBe(true);
    // The server restarted: its revisions start over.
    expect(newerMcp(status("a", 9), status("b", 1))).toBe(true);
  });

  it("an older read arriving after a newer event does not take the panel back", () => {
    const qc = new QueryClient();
    keepMcpStatus(qc, status("a", 5, "failed"));
    keepMcpStatus(qc, status("a", 4, "starting"));
    expect(qc.getQueryData<McpStatus>(keys.mcp)?.servers[0]?.state).toBe("failed");
    keepMcpStatus(qc, status("a", 6, "ready"));
    expect(qc.getQueryData<McpStatus>(keys.mcp)?.revision).toBe(6);
  });
});
