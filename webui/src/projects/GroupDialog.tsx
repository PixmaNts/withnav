import { UsersThree } from "@phosphor-icons/react";
import { useEffect, useId, useRef, useState } from "react";
import type { MemoryGroup, MemoryProject } from "../api/types";
import "../memory/memory.css";

interface Props {
  project: MemoryProject;
  groups: MemoryGroup[];
  busy: boolean;
  error: string | null;
  /** The chosen group's id, or null for no group. */
  onAnswer: (groupId: string | null) => void;
  onCreate: (name: string) => void;
  /** Leaves the question open: it is asked again at the next start. */
  onLater: () => void;
}

/** The first-time question for a repository the register does not know yet. */
export function GroupDialog({ project, groups, busy, error, onAnswer, onCreate, onLater }: Props) {
  const [choice, setChoice] = useState<string | null>(null);
  const [name, setName] = useState("");
  const titleId = useId();
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => box.current?.querySelector<HTMLElement>("input")?.focus(), []);

  const ready = choice === "new" ? name.trim() !== "" : choice !== null;
  const save = () => {
    if (!ready || busy) return;
    if (choice === "new") onCreate(name.trim());
    else onAnswer(choice === "none" ? null : choice);
  };
  const repo = project.aliases[0];

  return (
    <div className="backdrop mm-fixed" onMouseDown={() => !busy && onLater()}>
      <div
        ref={box}
        className="confirm mm-group-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            if (!busy) onLater();
          }
        }}
      >
        <h2 id={titleId}>
          <UsersThree size={16} aria-hidden /> Which group does <span className="mono">{project.name}</span> belong to?
        </h2>
        {repo && <div className="mono mm-hint">{repo}</div>}
        <div className="mm-choices" role="radiogroup" aria-labelledby={titleId}>
          {groups.map((g) => (
            <label key={g.id}>
              <input type="radio" name="group" value={g.id} checked={choice === g.id} onChange={() => setChoice(g.id)} />
              {g.name}
            </label>
          ))}
          <label>
            <input type="radio" name="group" value="none" checked={choice === "none"} onChange={() => setChoice("none")} />
            No group
          </label>
          <label>
            <input type="radio" name="group" value="new" checked={choice === "new"} onChange={() => setChoice("new")} />
            New group
          </label>
          {choice === "new" && <input aria-label="New group name" placeholder="Group name" value={name} onChange={(e) => setName(e.target.value)} onKeyDown={(e) => e.key === "Enter" && (e.preventDefault(), save())} />}
        </div>
        <p className="mm-hint">Items scoped to the group are shared with its projects; global items are shared everywhere.</p>
        {error && (
          <div className="confirm-error" role="alert">
            {error}
          </div>
        )}
        <div className="confirm-actions">
          <button type="button" className="btn ghost" onClick={onLater} disabled={busy}>
            Ask me later
          </button>
          <button type="button" className="btn" onClick={save} disabled={!ready || busy}>
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
