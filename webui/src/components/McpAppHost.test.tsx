// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { McpAppHost } from "./McpAppHost";
import { api } from "../api/client";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
afterEach(() => vi.restoreAllMocks());

it("decodes a base64 blob resource as UTF-8", async () => {
  const html = "<p>Café ✓ 日本語 — 🚢</p>";
  const blob = btoa(String.fromCharCode(...new TextEncoder().encode(html)));
  vi.spyOn(api, "mcpResource").mockResolvedValue({ uri: "ui://draw/app", mime_type: "text/html", blob });
  const host = document.createElement("div");
  const root = createRoot(host);
  await act(async () => root.render(<McpAppHost app={{ callId: "c1", server: "draw", tool: "show", resourceUri: "ui://draw/app" }} expanded={false} onToggleExpand={() => {}} onClose={() => {}} />));
  expect(host.querySelector("iframe")!.getAttribute("srcdoc")).toBe(html);
  await act(async () => root.unmount());
});
