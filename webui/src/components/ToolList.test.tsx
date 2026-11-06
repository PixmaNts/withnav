// @vitest-environment jsdom
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";
import type { ToolCall } from "../session/reducer";
import { extractMcpApp, extractMcpMedia, ToolList } from "./ToolList";

describe("MCP media and MCP App extraction & rendering", () => {
  const sampleCall: ToolCall = {
    id: "call-1",
    name: "mcp__excalidraw__get_canvas_screenshot",
    args: { scale: 1 },
    status: "ok",
    output: "[image: image/png, 3 bytes]\nCanvas screenshot captured.",
    metadata: {
      mcp_server: "excalidraw",
      mcp_tool: "get_canvas_screenshot",
      mcp_media: [{ mime_type: "image/png", data: "AAAA" }],
      mcp_ui: {
        server: "excalidraw",
        tool: "get_canvas_screenshot",
        resource_uri: "ui://excalidraw/app.html",
        url: "http://127.0.0.1:3000",
      },
    },
  };

  it("extracts media and app descriptor from tool metadata", () => {
    expect(extractMcpMedia(sampleCall)).toEqual([{ mime_type: "image/png", data: "AAAA" }]);
    expect(extractMcpApp(sampleCall)).toEqual({
      callId: "call-1",
      server: "excalidraw",
      tool: "get_canvas_screenshot",
      resourceUri: "ui://excalidraw/app.html",
      html: undefined,
      url: "http://127.0.0.1:3000",
      args: { scale: 1 },
      output: sampleCall.output,
      metadata: sampleCall.metadata,
    });
  });

  it("renders inline thumbnail and triggers onOpenApp when Open App is clicked", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const onOpenApp = vi.fn();
    const root = createRoot(host);
    root.render(<ToolList calls={[sampleCall]} onOpenApp={onOpenApp} />);
    await new Promise((r) => setTimeout(r, 20));

    const img = host.querySelector<HTMLImageElement>(".mcp-media-card img");
    expect(img).not.toBeNull();
    expect(img?.getAttribute("src")).toBe("data:image/png;base64,AAAA");

    const openBtn = host.querySelector<HTMLButtonElement>(".mcp-open-btn");
    expect(openBtn).not.toBeNull();
    openBtn?.click();
    expect(onOpenApp).toHaveBeenCalledTimes(1);
    expect(onOpenApp.mock.calls[0]?.[0]?.server).toBe("excalidraw");

    root.unmount();
    host.remove();
  });
});
