import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../api/client";
import { registerChanged, useProjects } from "../api/queries";
import { describeError } from "../memory/model";
import { GroupDialog } from "./GroupDialog";

/**
 * Asks, once per start, about the first project whose group is still unanswered. "Ask me later"
 * tells the register to keep the question open and hides it until the page is loaded again.
 */
export function GroupQuestion() {
  const qc = useQueryClient();
  const register = useProjects();
  const [later, setLater] = useState<ReadonlySet<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const project = register.data?.projects.find((p) => !p.group_answered && !later.has(p.id));
  if (!project) return null;

  const hide = () => setLater((held) => new Set(held).add(project.id));
  const run = async (work: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await work();
      await registerChanged(qc);
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <GroupDialog
      key={project.id}
      project={project}
      groups={register.data?.groups ?? []}
      busy={busy}
      error={error}
      onAnswer={(group) => void run(() => api.answerGroup(project.id, { group_id: group }))}
      onCreate={(name) =>
        void run(async () => {
          const group = await api.createGroup(name);
          await api.answerGroup(project.id, { group_id: group.id });
        })
      }
      onLater={() => {
        hide();
        // A failed note to the register changes nothing: the question stays open there either way.
        void api.answerGroup(project.id, { group_id: null, later: true }).catch(() => undefined);
      }}
    />
  );
}
