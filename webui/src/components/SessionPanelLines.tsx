import { useQueryClient } from "@tanstack/react-query";
import { Target } from "@phosphor-icons/react";
import { useState } from "react";
import { api } from "../api/client";
import { registerChanged } from "../api/queries";
import type { Item } from "../session/reducer";
import { ago } from "./SessionPanel";

type Remembered = Extract<Item, { kind: "remembered" }>;
type GoalNote = Extract<Item, { kind: "goal-note" }>;

/**
 * `Remembered: "<title>" · <scope> · undo`. Undo invalidates the item at the version `/remember`
 * stored; the line then offers to restore it. A version the register has moved past is refused by
 * the server, and the line says so instead of undoing something newer.
 */
export function RememberedLine({ item, readOnly }: { item: Remembered; readOnly?: boolean }) {
  const qc = useQueryClient();
  // The version the next undo or redo names: each change of the item raises it by one.
  const [version, setVersion] = useState(item.version);
  const [removed, setRemoved] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function toggle() {
    setBusy(true);
    setError(null);
    try {
      const next = removed ? await api.restoreMemory(item.itemId, { expected_version: version }) : await api.invalidateMemory(item.itemId, { expected_version: version });
      setVersion(next.current_version);
      setRemoved(!removed);
      await registerChanged(qc);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="remembered">
      <span>{removed ? "Removed from memory" : "Remembered"}: “{item.title}”</span>
      <span className="muted"> · {item.scope}</span>
      {!readOnly && (
        <>
          <span className="muted"> · </span>
          <button type="button" className="link-btn" disabled={busy} onClick={() => void toggle()}>
            {removed ? "redo" : "undo"}
          </button>
        </>
      )}
      {item.notice && <div className="muted">{item.notice}</div>}
      {error && <div className="remembered-error" role="alert">{error}</div>}
    </div>
  );
}

/** What a goal check told the agent: marked as an automatic observation, never a Captain message. */
export function GoalNoteBlock({ item }: { item: GoalNote }) {
  const at = Date.parse(item.at);
  return (
    <div className="goal-note">
      <div className="goal-note-head">
        <Target size={13} aria-hidden />
        Goal note
        <span className="muted">· automatic observation, not a Captain message</span>
        {!Number.isNaN(at) && <span className="right muted" title={new Date(at).toLocaleString()}>{ago(at)}</span>}
      </div>
      <div className="goal-note-text">{item.text}</div>
    </div>
  );
}
