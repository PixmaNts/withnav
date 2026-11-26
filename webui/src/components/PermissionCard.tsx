import { FilePlus, PencilSimple, ShieldCheck, Terminal } from "@phosphor-icons/react";
import { createContext, useContext, useState } from "react";
import type { Permission, PermissionReview } from "../session/reducer";

type Decision = "allow_once" | "allow_always" | "reject";

/**
 * "Hold for me": stops the parallel review of a request and keeps the card for the Captain. It is
 * a context, not a prop of the answer callback, so the conversation view that renders the card
 * does not have to know the review exists.
 */
export const PermissionHoldContext = createContext<{ hold: (requestId: string) => Promise<void> } | null>(null);

/** What the call would do, as the Captain reviews it: path and content, or the command. */
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

/** The server supplies the scope and rule shown on the Always answer. */
export function PermissionCard({
  perm,
  onAnswer,
  readOnly,
}: {
  perm: Permission;
  onAnswer: (decision: Decision, message?: string) => Promise<void>;
  readOnly?: boolean;
}) {
  const holder = useContext(PermissionHoldContext);
  const [sending, setSending] = useState<Decision | "hold" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState("");
  const view = preview(perm);
  const Icon = ICONS[perm.tool as keyof typeof ICONS] ?? ShieldCheck;
  const count = addedLines(perm);
  const offersAlways = perm.alwaysLabel !== undefined;
  const site = perm.scope.kind === "origin";
  const answer = async (decision: Decision) => {
    setSending(decision);
    setError(null);
    try {
      await onAnswer(decision, decision === "reject" ? message.trim() || undefined : undefined);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSending(null);
    }
  };
  const hold = async () => {
    if (!holder) return;
    setSending("hold");
    setError(null);
    try {
      await holder.hold(perm.requestId);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setSending(null);
  };
  const waiting = !perm.resolution;
  const canHold = waiting && !readOnly && !!holder && perm.review?.state === "reviewing";
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
      {waiting && perm.review && <ReviewLine review={perm.review} />}
      {waiting && !readOnly && (
        <div className="actions">
          <button type="button" className="btn" disabled={!!sending} onClick={() => answer("allow_once")} autoFocus>
            Once
          </button>
          {offersAlways && (
            <button type="button" className="btn ghost" disabled={!!sending} onClick={() => answer("allow_always")}>
              {perm.alwaysLabel}
            </button>
          )}
          <button type="button" className="btn ghost" disabled={!!sending} onClick={() => answer("reject")}>
            Reject
          </button>
          <div className="question-custom"><input aria-label="Reject message (optional)" placeholder="Reject message (optional)" value={message} disabled={!!sending} onChange={(event) => {
            if (new TextEncoder().encode(event.target.value).length > 2048) {
              setError("Reject message must be at most 2048 UTF-8 bytes");
              return;
            }
            setError(null);
            setMessage(event.target.value);
          }} /><span className="muted" role="status">{new TextEncoder().encode(message).length} / 2048 UTF-8 bytes</span></div>
          {canHold && (
            <button type="button" className="btn ghost" disabled={!!sending} onClick={() => void hold()}>
              Hold for me
            </button>
          )}
          {sending && <span className="muted">Sending…</span>}
        </div>
      )}
      {waiting && readOnly && <div className="outcome">Waiting for an answer in the parent conversation.</div>}
      {error && <div className="composer-error">{error}</div>}
      {!waiting && <Outcome perm={perm} site={site} />}
      {!waiting && perm.approvedBy === "metacognition" && perm.review && <ReviewText review={perm.review} />}
      {!waiting && perm.reviewCancelled && <div className="review muted" role="status">Review cancelled: you answered first.</div>}
    </div>
  );
}

/** The review's short conclusion, the part of the request it relied on and the consequence. */
function ReviewText({ review }: { review: PermissionReview }) {
  return (
    <div className="review-text">
      {review.conclusion && <div>{review.conclusion}</div>}
      {review.basis && <div className="muted">Relied on: {review.basis}</div>}
      {review.consequence && <div className="muted">Consequence: {review.consequence}</div>}
    </div>
  );
}

/** Where the review of a waiting request stands; the Captain's buttons stay active in every state. */
function ReviewLine({ review }: { review: PermissionReview }) {
  switch (review.state) {
    case "reviewing":
      return <div className="review muted" role="status"><span className="spin" aria-hidden>⠋</span> Metacognition is reviewing…</div>;
    case "wait":
      return (
        <div className="review wait" role="status">
          <span className="bypass-badge">Waiting for you</span>
          <ReviewText review={review} />
        </div>
      );
    case "held":
      return <div className="review muted" role="status">Held for you: the review stopped.</div>;
    case "unavailable":
      return <div className="review muted" role="status">No review; your call.</div>;
    default:
      return null;
  }
}

function Outcome({ perm, site }: { perm: Permission; site: boolean }) {
  // Metacognition only ever allows a call once, whatever the resolution word says.
  if (perm.approvedBy === "metacognition" && perm.resolution?.startsWith("allow")) {
    return <div className="outcome"><span className="ok">✓</span> Allowed once by Metacognition.</div>;
  }
  switch (perm.resolution) {
    case "allow_once":
      return <div className="outcome"><span className="ok">✓</span> Allowed once.</div>;
    case "allow_skill_machine":
      return <div className="outcome"><span className="ok">✓</span> Allowed this user skill's script on this machine.</div>;
    case "allow_config_session":
      return <div className="outcome"><span className="ok">✓</span> Allowed WithNav config for this session.</div>;
    case "allow_folder":
      return <div className="outcome"><span className="ok">✓</span> Allowed for this {site ? "site" : "folder"}.</div>;
    case "allow_always":
      if (perm.alwaysSaved === false) return <div className="outcome"><span className="bad">✕</span> Always not saved: {perm.message ?? "unknown error"}; ran once</div>;
      return <div className="outcome"><span className="ok">✓</span> Allowed always.</div>;
    case "reject":
    case "deny":
      return <div className="outcome"><span className="bad">✕</span> Denied.{perm.message && perm.message !== "the user denied this tool call" ? ` ${perm.message}` : " Nav proposes an alternative."}</div>;
    case "cancelled":
      return <div className="outcome">Cancelled: the turn stopped before an answer.</div>;
    case "expired":
      return <div className="outcome">Expired: the session restarted.</div>;
    default:
      return <div className="outcome">{perm.resolution}</div>;
  }
}
