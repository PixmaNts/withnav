import { Check, TreeStructure, Warning, X } from "@phosphor-icons/react";
import { useState } from "react";
import type { Child, ToolCall } from "../session/reducer";
import { toolLine } from "../session/toolText";
import { Spinner } from "./Spinner";

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

export function ToolList({
  calls,
  children,
  onOpenChild,
  childCalls,
}: {
  calls: ToolCall[];
  children?: Record<string, Child>;
  onOpenChild?: (id: string) => void;
  /** Tool-call counts of running children, when known. */
  childCalls?: Record<string, number>;
}) {
  const [open, setOpen] = useState<string | null>(null);
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
                <span className="pu">@{agent}</span>
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
        return (
          <div className="tool-wrap" key={call.id}>
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
            {expanded && <pre className="tool-out">{call.output}</pre>}
          </div>
        );
      })}
    </div>
  );
}
