import { ArrowsInSimple, ArrowsOutSimple, ArrowClockwise, ArrowSquareOut, X } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import { api } from "../api/client";
import type { McpAppDescriptor } from "../api/types";

export function McpAppHost({
  app,
  expanded,
  onToggleExpand,
  onClose,
}: {
  app: McpAppDescriptor;
  expanded: boolean;
  onToggleExpand: () => void;
  onClose: () => void;
}) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const [html, setHtml] = useState<string | null>(app.html ?? null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    let cancelled = false;
    if (app.html) {
      setHtml(app.html);
      setError(null);
      setLoading(false);
      return;
    }
    if (!app.resourceUri) {
      setHtml(null);
      setError(null);
      setLoading(false);
      return;
    }
    setLoading(true);
    setError(null);
    api
      .mcpResource(app.server, app.resourceUri)
      .then((res) => {
        if (cancelled) return;
        if (res.text) {
          setHtml(res.text);
        } else if (res.blob) {
          // `atob` gives one character per byte: decoding those bytes as UTF-8 keeps non-ASCII text intact.
          setHtml(new TextDecoder("utf-8").decode(Uint8Array.from(atob(res.blob), (c) => c.charCodeAt(0))));
        } else {
          setError("MCP resource returned no HTML content.");
        }
      })
      .catch((err) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [app.server, app.resourceUri, app.html, reloadKey]);

  // Standard MCP Apps JSON-RPC 2.0 postMessage bridge for ui:// resources.
  useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      const win = iframeRef.current?.contentWindow;
      if (!win || event.source !== win) return;
      const msg = event.data;
      if (!msg || typeof msg !== "object" || msg.jsonrpc !== "2.0") return;

      const reply = (id: unknown, result?: unknown, rpcError?: { code: number; message: string }) => {
        if (id === undefined || id === null) return;
        win.postMessage(
          rpcError ? { jsonrpc: "2.0", id, error: rpcError } : { jsonrpc: "2.0", id, result },
          "*",
        );
      };

      const notify = (method: string, params: unknown) => {
        win.postMessage({ jsonrpc: "2.0", method, params }, "*");
      };

      const structuredContent = app.metadata?.mcp_structured ?? app.metadata ?? {};

      if (msg.method === "ui/initialize") {
        reply(msg.id, {
          protocolVersion: "2025-11-25",
          hostCapabilities: { openLinks: {}, serverTools: {}, serverResources: {} },
          hostInfo: { name: "WithNav", version: "0.8.2" },
          hostContext: {
            toolInfo: { id: app.callId, tool: { name: app.tool } },
          },
        });
        notify("ui/notifications/tool-input", { arguments: app.args ?? {} });
        if (app.output !== undefined || app.metadata?.mcp_structured !== undefined) {
          notify("ui/notifications/tool-result", {
            content: [{ type: "text", text: app.output ?? "" }],
            structuredContent,
          });
        }
        return;
      }

      if (msg.method === "ui/notifications/initialized") {
        notify("ui/notifications/tool-input", { arguments: app.args ?? {} });
        if (app.output !== undefined || app.metadata?.mcp_structured !== undefined) {
          notify("ui/notifications/tool-result", {
            content: [{ type: "text", text: app.output ?? "" }],
            structuredContent,
          });
        }
        return;
      }

      if (msg.method === "ui/open-link" || msg.method === "ui/openLink") {
        const url = typeof msg.params?.url === "string" ? msg.params.url : "";
        if (url.startsWith("http://") || url.startsWith("https://")) {
          window.open(url, "_blank", "noopener,noreferrer");
          reply(msg.id, {});
        } else {
          reply(msg.id, undefined, { code: -32602, message: "Invalid link URL" });
        }
        return;
      }

      if (msg.method === "tools/call") {
        const name = typeof msg.params?.name === "string" ? msg.params.name : "";
        const args = msg.params?.arguments && typeof msg.params.arguments === "object" ? msg.params.arguments : {};
        if (!name) {
          reply(msg.id, undefined, { code: -32602, message: "Missing tool name" });
          return;
        }
        void api
          .mcpCall(app.server, name, args)
          .then((result) => reply(msg.id, result))
          .catch((err) =>
            reply(msg.id, undefined, {
              code: -32000,
              message: err instanceof Error ? err.message : String(err),
            }),
          );
        return;
      }

      if (msg.method === "resources/read") {
        const uri = typeof msg.params?.uri === "string" ? msg.params.uri : "";
        if (!uri) {
          reply(msg.id, undefined, { code: -32602, message: "Missing resource uri" });
          return;
        }
        void api
          .mcpResource(app.server, uri)
          .then((res) =>
            reply(msg.id, {
              contents: [
                {
                  uri: res.uri,
                  mimeType: res.mime_type,
                  ...(res.text ? { text: res.text } : {}),
                  ...(res.blob ? { blob: res.blob } : {}),
                },
              ],
            }),
          )
          .catch((err) =>
            reply(msg.id, undefined, {
              code: -32000,
              message: err instanceof Error ? err.message : String(err),
            }),
          );
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [app]);

  const showUrlFallback = !html && !loading && !!app.url;

  return (
    <div className="mcp-app-host">
      <div className="mcp-app-toolbar">
        <div className="mcp-app-title" title={app.resourceUri ?? app.url}>
          <span className="mono">{app.server}</span>
          <span className="muted"> · {app.tool}</span>
        </div>
        <div className="mcp-app-actions">
          {app.url && (
            <a
              href={app.url}
              target="_blank"
              rel="noreferrer"
              className="icon-btn"
              title={`Open ${app.url} in browser`}
              aria-label="Open in new tab"
            >
              <ArrowSquareOut size={14} />
            </a>
          )}
          <button
            type="button"
            className="icon-btn"
            onClick={() => setReloadKey((k) => k + 1)}
            title="Reload MCP App"
            aria-label="Reload MCP App"
          >
            <ArrowClockwise size={14} />
          </button>
          <button
            type="button"
            className="icon-btn"
            onClick={onToggleExpand}
            title={expanded ? "Shrink panel" : "Expand panel"}
            aria-label={expanded ? "Shrink panel" : "Expand panel"}
          >
            {expanded ? <ArrowsInSimple size={14} /> : <ArrowsOutSimple size={14} />}
          </button>
          <button
            type="button"
            className="icon-btn"
            onClick={onClose}
            title="Close MCP App"
            aria-label="Close MCP App"
          >
            <X size={14} />
          </button>
        </div>
      </div>
      {loading && <div className="panel-empty">Loading MCP App ({app.server})…</div>}
      {error && !showUrlFallback && <div className="panel-empty bad">{error}</div>}
      {html && (
        <iframe
          key={`srcdoc:${reloadKey}:${app.callId}`}
          ref={iframeRef}
          className="mcp-app-frame"
          sandbox="allow-scripts allow-forms"
          srcDoc={html}
          title={`${app.server}: ${app.tool}`}
        />
      )}
      {showUrlFallback && (
        <iframe
          key={`url:${reloadKey}:${app.url}`}
          ref={iframeRef}
          className="mcp-app-frame"
          sandbox="allow-scripts allow-same-origin allow-forms allow-downloads"
          src={app.url}
          title={`${app.server}: ${app.tool}`}
        />
      )}
    </div>
  );
}
