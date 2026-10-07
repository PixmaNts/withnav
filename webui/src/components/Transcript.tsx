import { ArrowUp, CaretRight, Compass, TreeStructure } from "@phosphor-icons/react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { readDelegation } from "../lib/compose";
import type { Item, Permission, SessionView, Turn } from "../session/reducer";
import { Markdown } from "./Markdown";
import { PermissionCard } from "./PermissionCard";
import { ToolList } from "./ToolList";

type Answer = (requestId: string, decision: "allow_once" | "allow_folder" | "deny") => Promise<void>;

interface Props {
  view: SessionView;
  /** The session's agent per the server, for turns the stream did not label. */
  agent?: string;
  onAnswer: Answer;
  onOpenChild: (id: string) => void;
  readOnly?: boolean;
  empty?: React.ReactNode;
}

/** Follows the newest content unless the driver scrolled up to read. */
function useStickToBottom(dep: unknown) {
  const ref = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onScroll = () => {
      stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    return () => el.removeEventListener("scroll", onScroll);
  }, []);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [dep]);
  return ref;
}

export function Transcript({ view, agent, onAnswer, onOpenChild, readOnly, empty }: Props) {
  const ref = useStickToBottom(view);
  return (
    <div className="transcript" ref={ref} role="log" aria-live="polite" aria-relevant="additions">
      {view.items.length === 0 && empty}
      {view.items.map((item) => (
        <ItemView key={item.key} item={item} view={view} agent={agent} onAnswer={onAnswer} onOpenChild={onOpenChild} readOnly={readOnly} />
      ))}
    </div>
  );
}

function ItemView({ item, view, agent, onAnswer, onOpenChild, readOnly }: { item: Item; view: SessionView; agent?: string; onAnswer: Answer; onOpenChild: (id: string) => void; readOnly?: boolean }) {
  switch (item.kind) {
    case "driver": {
      const { delegated, text } = readDelegation(item.text);
      return (
        <div className="msg-driver">
          <div className="byline">
            Driver{delegated ? " · Delegation" : ""}
            {item.skill ? ` · skill ${item.skill}` : ""}
          </div>
          <Markdown text={text} />
        </div>
      );
    }
    case "wake":
      return <Wake item={item} />;
    case "subagent-result":
      return (
        <div className="sub-result">
          <div className="head">
            <TreeStructure size={13} aria-hidden />
            subagent.done · @{item.agent ?? (item.source.replace(/^subagent:?/, "") || "subagent")}
            {item.outcome ? ` · ${item.outcome}` : ""}
            {item.childId && (
              <button type="button" className="link-btn" style={{ marginLeft: "auto" }} onClick={() => onOpenChild(item.childId!)}>
                Open
              </button>
            )}
          </div>
          <div className="muted" style={{ fontSize: 11.5 }}>Result passed to the cognition as untrusted text.</div>
          <Markdown text={item.text} />
        </div>
      );
    case "turn":
      return <TurnView turn={item} view={view} fallbackAgent={agent} onAnswer={onAnswer} onOpenChild={onOpenChild} readOnly={readOnly} />;
    case "permission": {
      const perm = view.permissions[item.requestId];
      return perm ? <PermissionBlock perm={perm} onAnswer={onAnswer} readOnly={readOnly} /> : null;
    }
    case "notice":
      return <div className={`notice${item.tone === "error" ? " error" : ""}`}>{item.text}</div>;
  }
}

function PermissionBlock({ perm, onAnswer, readOnly }: { perm: Permission; onAnswer: Answer; readOnly?: boolean }) {
  return <PermissionCard perm={perm} onAnswer={(decision) => onAnswer(perm.requestId, decision)} readOnly={readOnly} />;
}

function TurnView({ turn, view, fallbackAgent, onAnswer, onOpenChild, readOnly }: { turn: Turn; view: SessionView; fallbackAgent?: string; onAnswer: Answer; onOpenChild: (id: string) => void; readOnly?: boolean }) {
  const agent = turn.agent ?? view.agent ?? fallbackAgent;
  const childCalls: Record<string, number> = {};
  return (
    <div className={`msg-cog${agent === "plan" ? " plan" : ""}`}>
      <div className="byline">Cognition{agent ? ` · ${agent}` : ""}</div>
      {turn.parts.map((part, i) => {
        switch (part.type) {
          case "reasoning": {
            const text = turn.reasoning[part.stepId];
            return text ? <Reasoning key={`r${i}`} text={text} live={turn.status === "running"} /> : null;
          }
          case "text":
            return <Markdown key={`t${i}`} text={part.text} />;
          case "tools":
            return (
              <ToolList
                key={`c${i}`}
                calls={part.calls.map((id) => view.tools[id]).filter((c) => c !== undefined)}
                children={view.children}
                onOpenChild={onOpenChild}
                childCalls={childCalls}
              />
            );
          case "permission": {
            const perm = view.permissions[part.requestId];
            return perm ? <PermissionBlock key={`p${i}`} perm={perm} onAnswer={onAnswer} readOnly={readOnly} /> : null;
          }
          case "question":
            return (
              <div className="question" key={`q${i}`}>
                <div className="label">Question</div>
                <Markdown text={part.text} />
              </div>
            );
        }
      })}
      {turn.status === "error" && <div className="turn-error">{turn.error || "The turn failed."}</div>}
      {turn.status === "interrupted" && <div className="turn-note">Stopped.</div>}
    </div>
  );
}

function Reasoning({ text, live }: { text: string; live: boolean }) {
  return (
    <details className="reasoning">
      <summary>
        <CaretRight size={11} aria-hidden />
        {live ? "Reasoning…" : "Reasoning"}
      </summary>
      <div className="body">{text}</div>
    </details>
  );
}

function Wake({ item }: { item: Extract<Item, { kind: "wake" }> }) {
  const [open, setOpen] = useState(false);
  const { note } = item;
  const where = note.path ? `${note.path}${note.line ? `:${note.line}` : ""}` : "";
  const hasDetails = !!(note.lead || note.diff || note.intent || note.rest);
  return (
    <div className="wake">
      <div className="wake-head">
        <ArrowUp size={13} aria-hidden />
        Wake · metacognition → cognition
        <span className="right">{[where, note.score ? `score ${note.score}` : ""].filter(Boolean).join(" · ")}</span>
      </div>
      <div>{note.reason ? capitalize(note.reason) : "The navigator raised something for the cognition."}</div>
      {hasDetails && (
        <button type="button" className="link-btn" aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? "Hide details" : note.diff ? "Show change" : "Show details"}
        </button>
      )}
      {open && (
        <div style={{ marginTop: 6 }}>
          {note.lead && <p style={{ margin: "0 0 6px" }}>{note.lead}</p>}
          {note.diff && (
            <div className="preview">
              {note.diff.split("\n").map((line, i) => (
                <div key={i} className={line.startsWith("+") && !line.startsWith("+++") ? "add" : line.startsWith("-") && !line.startsWith("---") ? "del" : "ctx"}>
                  {line || " "}
                </div>
              ))}
            </div>
          )}
          {note.intent && (
            <>
              <div className="section-label">INTENT ON RECORD</div>
              <Markdown text={note.intent} />
            </>
          )}
          {note.rest && <Markdown text={note.rest} />}
        </div>
      )}
    </div>
  );
}

const capitalize = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

export function EmptySession({ title, children }: { title: string; children?: React.ReactNode }) {
  return (
    <div className="empty">
      <Compass size={26} aria-hidden />
      <h2>{title}</h2>
      {children}
    </div>
  );
}
