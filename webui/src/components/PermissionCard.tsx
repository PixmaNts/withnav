import { FilePlus, PencilSimple, ShieldCheck, Terminal } from "@phosphor-icons/react";
import { useState } from "react";
import type { Permission } from "../session/reducer";

type Decision = "allow_once" | "allow_folder" | "deny";

/** What the call would do, as the driver reviews it: path and content, or the command. */
function preview(perm: Permission): { title: string; path?: string; lines: { kind: "add" | "del" | "ctx"; text: string }[]; extra?: string } {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let args: any;
  try {
    args = JSON.parse(perm.arguments);
  } catch {
    return { title: perm.tool, lines: [{ kind: "ctx", text: perm.arguments }], extra: "Arguments shortened by the server; the full call is not shown." };
  }
  const s = (key: string) => (typeof args?.[key] === "string" ? (args[key] as string) : "");
  const cap = (text: string) => text.split("\n").slice(0, 40);
  switch (perm.tool) {
    case "bash":
      return { title: "Run", lines: [{ kind: "ctx", text: `$ ${s("command")}` }] };
    case "write":
      return { title: "Write", path: s("path"), lines: cap(s("content")).map((text) => ({ kind: "add", text: `+ ${text}` })) };
    case "edit":
      return {
        title: "Edit",
        path: s("path"),
        lines: [...cap(s("old_string")).map((text) => ({ kind: "del" as const, text: `- ${text}` })), ...cap(s("new_string")).map((text) => ({ kind: "add" as const, text: `+ ${text}` }))],
      };
    default:
      return { title: perm.tool, lines: [{ kind: "ctx", text: JSON.stringify(args, null, 2) }] };
  }
}

function addedLines(perm: Permission): number | null {
  try {
    const args = JSON.parse(perm.arguments);
    if (perm.tool === "write" && typeof args.content === "string") return args.content.split("\n").length;
    if (perm.tool === "edit" && typeof args.new_string === "string") return args.new_string.split("\n").length;
  } catch {
    /* shortened arguments: no count */
  }
  return null;
}

const ICONS = { write: FilePlus, edit: PencilSimple, bash: Terminal } as const;

/**
 * The permission prompt of the design: allow once / always for this folder /
 * deny. "Always" is offered only where the server honours it (`fetch`, never
 * for the plan agent: `src/agent/permissions.rs::offered_decision` turns any
 * other "allow folder" into a single approval), so the button never lies.
 */
export function PermissionCard({
  perm,
  onAnswer,
  readOnly,
}: {
  perm: Permission;
  onAnswer: (decision: Decision) => Promise<void>;
  readOnly?: boolean;
}) {
  const [sending, setSending] = useState<Decision | null>(null);
  const [error, setError] = useState<string | null>(null);
  const view = preview(perm);
  const Icon = ICONS[perm.tool as keyof typeof ICONS] ?? ShieldCheck;
  const count = addedLines(perm);
  const offersAlways = perm.agent !== "plan" && perm.tool === "fetch";
  const site = perm.scope.kind === "origin";
  const answer = async (decision: Decision) => {
    setSending(decision);
    setError(null);
    try {
      await onAnswer(decision);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSending(null);
    }
  };
  const waiting = !perm.resolution;
  return (
    <div className={`perm${waiting ? " waiting" : ""}`} role="group" aria-label={`Permission: ${perm.agent} wants to use ${perm.tool}`}>
      <div className="perm-head">
        <Icon size={15} aria-hidden />
        <span>{view.title}</span>
        {view.path && <span className="mono path">{view.path}</span>}
        {count !== null && <span className="muted">+{count}</span>}
      </div>
      {perm.reason && <div className="perm-reason">{perm.reason}</div>}
      <div className="preview">
        {view.lines.map((line, i) => (
          <div key={i} className={line.kind}>
            {line.text || " "}
          </div>
        ))}
      </div>
      {view.extra && <div className="perm-reason">{view.extra}</div>}
      {waiting && !readOnly && (
        <div className="actions">
          <button type="button" className="btn" disabled={!!sending} onClick={() => answer("allow_once")} autoFocus>
            Allow once
          </button>
          {offersAlways && (
            <button type="button" className="btn ghost" disabled={!!sending} onClick={() => answer("allow_folder")}>
              {site ? "Always for this site" : "Always for this folder"}
            </button>
          )}
          <button type="button" className="btn ghost" disabled={!!sending} onClick={() => answer("deny")}>
            Deny
          </button>
          {sending && <span className="muted">Sending…</span>}
        </div>
      )}
      {waiting && readOnly && <div className="outcome">Waiting for an answer in the parent conversation.</div>}
      {error && <div className="composer-error">{error}</div>}
      {!waiting && <Outcome perm={perm} site={site} />}
    </div>
  );
}

function Outcome({ perm, site }: { perm: Permission; site: boolean }) {
  switch (perm.resolution) {
    case "allow_once":
      return <div className="outcome"><span className="ok">✓</span> Allowed once.</div>;
    case "allow_folder":
      return <div className="outcome"><span className="ok">✓</span> Allowed for this {site ? "site" : "folder"}.</div>;
    case "deny":
      return <div className="outcome"><span className="bad">✕</span> Denied.{perm.message && perm.message !== "the user denied this tool call" ? ` ${perm.message}` : " Cognition proposes an alternative."}</div>;
    case "cancelled":
      return <div className="outcome">Cancelled: the turn stopped before an answer.</div>;
    case "expired":
      return <div className="outcome">Expired: the session restarted.</div>;
    default:
      return <div className="outcome">{perm.resolution}</div>;
  }
}
