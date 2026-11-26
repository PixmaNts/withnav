import { ShareNetwork } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import type { MemoryProject } from "../api/types";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { plural } from "../lib/format";

/** Asks the Captain to confirm a change that lets more projects read something; lists them. */
export function WideningDialog({
  title,
  lead,
  projects,
  busy,
  error,
  onConfirm,
  onDismiss,
}: {
  title: string;
  lead: ReactNode;
  projects: MemoryProject[];
  busy: boolean;
  error: string | null;
  onConfirm: () => void;
  onDismiss: () => void;
}) {
  return (
    <ConfirmDialog title={title} icon={<ShareNetwork size={16} aria-hidden />} danger={false} busy={busy} error={error} confirmLabel="Share" onConfirm={onConfirm} onDismiss={onDismiss}>
      <p className="confirm-lead">{lead}</p>
      {projects.length ? (
        <>
          <p>{plural(projects.length, "project")} would start to read it:</p>
          <ul className="mm-affected">
            {projects.map((p) => (
              <li key={p.id}>
                <b>{p.name}</b>
                {p.aliases[0] && <span className="mono muted"> {p.aliases[0]}</span>}
              </li>
            ))}
          </ul>
        </>
      ) : (
        <p>No project reads it yet; it would only be shared from now on.</p>
      )}
    </ConfirmDialog>
  );
}
