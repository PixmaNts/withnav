import { Crew } from "./Crew";
import { AppWindow, Check, TreeStructure, Warning, X } from "@phosphor-icons/react";
import { useState } from "react";
import type { McpAppDescriptor, McpMediaItem, McpUiMeta } from "../api/types";
import type { Child, ToolCall } from "../session/reducer";
import { toolLine } from "../session/toolText";
import { Spinner } from "./Spinner";
import { Thumbnails } from "./Images";
import type { ImageRef } from "../api/types";

/** The images a tool returned, as the server stored them (`metadata.images`). */
function toolImages(call: ToolCall): ImageRef[] {
  const images = call.metadata?.images;
  return Array.isArray(images) ? images.filter((i): i is ImageRef => typeof i?.attachment_id === "string") : [];
}

function StatusMark({ call }: { call: ToolCall }) {
  if (call.status === "running") return <Spinner />;
  if (call.status === "ok") return <Check className="ok" size={14} aria-label="done" />;
  if (call.status === "denied") return <X className="bad" size={14} aria-label="denied" />;
  if (call.status === "interrupted") return <X size={14} aria-label="interrupted" />;
  return <Warning className="bad" size={14} aria-label="failed" />;
}

/** A spawned subagent's child id, read from the `task` tool's JSON answer. */
export function taskChildId(call: ToolCall): string | undefined {
  if (call.name !== "task" || !call.output) return undefined;
  try {
    const value = JSON.parse(call.output);
    return typeof value?.child_id === "string" ? value.child_id : undefined;
  } catch {
    return undefined;
  }
}

export function extractMcpMedia(call: ToolCall): McpMediaItem[] {
  const raw = call.metadata?.mcp_media;
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (m): m is McpMediaItem =>
      !!m && typeof m.mime_type === "string" && (typeof m.data === "string" || typeof m.svg === "string"),
  );
}

export function extractMcpApp(call: ToolCall): McpAppDescriptor | null {
  const ui = call.metadata?.mcp_ui as McpUiMeta | undefined;
  if (!ui || typeof ui.server !== "string") return null;
  if (!ui.resource_uri && !ui.html && !ui.url) return null;
  return {
    callId: call.id,
    server: ui.server,
    tool: typeof ui.tool === "string" ? ui.tool : call.name,
    resourceUri: typeof ui.resource_uri === "string" ? ui.resource_uri : undefined,
    html: typeof ui.html === "string" ? ui.html : undefined,
    url: typeof ui.url === "string" ? ui.url : undefined,
    args: call.args,
    output: call.output,
    metadata: call.metadata,
  };
}

function mediaSrc(item: McpMediaItem): string {
  if (item.svg) return `data:image/svg+xml;utf8,${encodeURIComponent(item.svg)}`;
  return `data:${item.mime_type};base64,${item.data ?? ""}`;
}

export function ToolList({
  calls,
  children,
  onOpenChild,
  onOpenApp,
  childCalls,
}: {
  calls: ToolCall[];
  children?: Record<string, Child>;
  onOpenChild?: (id: string) => void;
  onOpenApp?: (app: McpAppDescriptor) => void;
  /** Tool-call counts of running children, when known. */
  childCalls?: Record<string, number>;
}) {
  const [open, setOpen] = useState<string | null>(null);
  const [zoomed, setZoomed] = useState<string | null>(null);
  if (!calls.length) return null;
  return (
    <div className="tools">
      {calls.map((call) => {
        const line = toolLine(call);
        const childId = taskChildId(call) ?? Object.values(children ?? {}).find((c) => c.toolCallId === call.id)?.id;
        if (call.name === "task") {
          const child = childId ? children?.[childId] : undefined;
          const agent = child?.agent ?? (typeof call.args?.agent === "string" ? call.args.agent : "subagent");
          const status = child?.status ?? (call.status === "ok" ? "running" : call.status);
          const count = childId ? childCalls?.[childId] : undefined;
          return (
            <div className="tool-wrap" key={call.id}>
              <button
                type="button"
                className="tool spawn expandable"
                disabled={!childId || !onOpenChild}
                onClick={() => childId && onOpenChild?.(childId)}
                title={childId ? `Open @${agent}` : undefined}
              >
                <TreeStructure className="pu" size={14} aria-hidden />
                <span>spawn_agent</span>
                <span className="pu"><Crew id={agent} /></span>
                <span className="meta muted">
                  {status}
                  {count !== undefined ? ` · ${count} ${count === 1 ? "tool" : "tools"}` : ""}
                </span>
              </button>
            </div>
          );
        }
        const expanded = open === call.id;
        const hasOutput = !!call.output && call.status !== "running";
        const media = extractMcpMedia(call);
        const app = extractMcpApp(call);
        return (
          <div className="tool-wrap" key={call.id}>
            <div className="tool-row-inline">
              <button
                type="button"
                className={`tool${hasOutput ? " expandable" : ""}`}
                aria-expanded={hasOutput ? expanded : undefined}
                onClick={() => hasOutput && setOpen(expanded ? null : call.id)}
                title={line.target}
              >
                <StatusMark call={call} />
                <span>{line.verb}</span>
                {line.target && <span className="target mono">{line.target}</span>}
                {line.meta && <span className="meta">{line.meta}</span>}
              </button>
              {app && onOpenApp && (
                <button
                  type="button"
                  className="mcp-open-btn"
                  onClick={() => onOpenApp(app)}
                  title={`Open ${app.server} (${app.tool}) in side panel`}
                >
                  <AppWindow size={13} aria-hidden />
                  <span>Open App</span>
                </button>
              )}
            </div>
            {media.length > 0 && (
              <div className="mcp-media-grid">
                {media.map((item, idx) => {
                  const key = `${call.id}:${idx}`;
                  const isZoomed = zoomed === key;
                  return (
                    <div key={key} className={`mcp-media-card${isZoomed ? " zoomed" : ""}`}>
                      <button
                        type="button"
                        className="mcp-media-zoom"
                        aria-pressed={isZoomed}
                        aria-label={`Enlarge ${call.name} preview ${idx + 1}`}
                        title={isZoomed ? "Shrink" : "Enlarge"}
                        onClick={() => setZoomed(isZoomed ? null : key)}
                      >
                        <img src={mediaSrc(item)} alt="" />
                      </button>
                    </div>
                  );
                })}
              </div>
            )}
            {expanded && <pre className="tool-out">{call.output}</pre>}
            <Thumbnails images={toolImages(call)} />
          </div>
        );
      })}
    </div>
  );
}
