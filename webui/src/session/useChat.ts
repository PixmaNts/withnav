// The session controller of the chat page: one conversation's stream, its
// server summary, and every action the driver can take on it. A route with no
// session id is a draft: agent, model and variant are kept locally and sent
// with `POST /v1/sessions` when the first message goes out.

import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useMemo, useState } from "react";
import { api } from "../api/client";
import { HttpError } from "../api/http";
import { keys, useAgents, useContextSnapshot, useModels, useSessionSummary } from "../api/queries";
import type { AgentEntry, ContentBlock, Envelope, ModelEntry } from "../api/types";
import { useSessionStream } from "./useSessionStream";

export interface Draft {
  agent: string | null;
  model: string | null;
  /** undefined = the model's default variant; null = no variant. */
  variant: string | null | undefined;
}

export interface Selection {
  agent: string;
  model: string | null;
  variant: string | null;
}

export function useChat(workspace: string | null, sessionId: string | null, onCreated: (id: string) => void) {
  const qc = useQueryClient();
  const summary = useSessionSummary(sessionId);
  const stopped = summary.data?.actor_state === "stopped";
  const onEnvelope = useCallback(
    (envelope: Envelope) => {
      if (envelope.kind === "model.changed" || envelope.kind === "session.agent_changed") {
        if (workspace) void qc.invalidateQueries({ queryKey: ["agents", workspace] });
      }
      if (envelope.kind === "session.end" && sessionId) {
        void qc.invalidateQueries({ queryKey: keys.session(sessionId) });
        if (workspace) void qc.invalidateQueries({ queryKey: keys.sessions(workspace) });
      }
      if (envelope.kind === "turn.end" && workspace) void qc.invalidateQueries({ queryKey: keys.sessions(workspace) });
      if ((envelope.kind === "subagent.spawned" || envelope.kind === "subagent.done") && workspace) void qc.invalidateQueries({ queryKey: keys.sessions(workspace) });
    },
    [qc, workspace, sessionId],
  );
  const stream = useSessionStream(sessionId, { ready: !!summary.data, once: stopped, onEnvelope });
  const agents = useAgents(workspace, sessionId);
  const models = useModels(workspace);
  const [draft, setDraft] = useState<Draft>({ agent: null, model: null, variant: undefined });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const defaultAgent = agents.data?.default_agent ?? models.data?.default_agent ?? "build";
  const contextQuery = useContextSnapshot(sessionId, !!summary.data && !stopped && stream.view.context === null);
  const view = stream.view.context === null && contextQuery.data ? { ...stream.view, context: contextQuery.data } : stream.view;
  const agentId = sessionId ? view.agent ?? summary.data?.agent ?? defaultAgent : draft.agent ?? defaultAgent;
  const agentEntry: AgentEntry | undefined = agents.data?.agents.find((a) => a.id === agentId);
  const chatModels: ModelEntry[] = useMemo(
    () => (models.data?.models ?? []).filter((m) => m.api === "chat" && m.enabled !== false),
    [models.data],
  );

  const selection: Selection = useMemo(() => {
    if (sessionId) {
      if (view.model !== null) return { agent: agentId, model: view.model, variant: view.variant };
      return { agent: agentId, model: summary.data?.model ?? null, variant: summary.data?.variant ?? null };
    }
    const base = agentEntry?.effective;
    const model = draft.model ?? base?.model ?? null;
    let variant: string | null;
    if (draft.variant !== undefined) variant = draft.variant;
    else if (draft.model) variant = chatModels.find((m) => m.name === draft.model)?.default_variant ?? null;
    else variant = base?.variant ?? null;
    return { agent: agentId, model, variant };
  }, [sessionId, view.model, view.variant, agentId, summary.data, agentEntry, draft, chatModels]);

  const readOnly = !!summary.data?.read_only || !!summary.data?.resume_disabled_reason;
  const running = view.activity === "running";

  const refreshSummary = useCallback(async () => {
    if (!sessionId) return;
    await qc.invalidateQueries({ queryKey: keys.session(sessionId) });
    await qc.refetchQueries({ queryKey: keys.session(sessionId) });
  }, [qc, sessionId]);

  /** A stopped conversation is resumed (same id) before it takes input. */
  const ensureLive = useCallback(async (): Promise<string | null> => {
    if (!sessionId || !workspace) return null;
    const state = qc.getQueryData<{ actor_state: string }>(keys.session(sessionId))?.actor_state;
    if (state === "stopped") {
      await api.create({ workspace, resume: sessionId });
      await refreshSummary();
    }
    return sessionId;
  }, [qc, sessionId, workspace, refreshSummary]);

  const withErrors = useCallback(async <T,>(work: () => Promise<T>): Promise<T | undefined> => {
    setError(null);
    try {
      return await work();
    } catch (e) {
      if (e instanceof HttpError && e.reason === "resume_required" && sessionId && workspace) {
        try {
          await api.create({ workspace, resume: sessionId });
          await refreshSummary();
          return await work();
        } catch (again) {
          setError(again instanceof Error ? again.message : String(again));
          return undefined;
        }
      }
      setError(e instanceof Error ? e.message : String(e));
      return undefined;
    }
  }, [sessionId, workspace, refreshSummary]);

  const send = useCallback(
    async (content: ContentBlock[]): Promise<boolean> => {
      if (!workspace) return false;
      setBusy(true);
      const ok = await withErrors(async () => {
        let id = sessionId;
        if (!id) {
          const created = await api.create({
            workspace,
            ...(draft.agent ? { agent: draft.agent } : {}),
            ...(draft.model ? { model: draft.model } : {}),
            ...(draft.variant !== undefined ? { variant: draft.variant } : {}),
          });
          id = created.session_id;
          setDraft({ agent: null, model: null, variant: undefined });
          onCreated(id);
        } else await ensureLive();
        await api.send(id, content);
        void qc.invalidateQueries({ queryKey: keys.sessions(workspace) });
        return true;
      });
      setBusy(false);
      return ok === true;
    },
    [workspace, sessionId, draft, withErrors, ensureLive, onCreated, qc],
  );

  const stop = useCallback(async () => {
    if (!sessionId) return;
    await withErrors(() => api.stop(sessionId));
    await refreshSummary();
  }, [sessionId, withErrors, refreshSummary]);

  /** Runs a server command; switches in a draft only change the draft. */
  const command = useCallback(
    async (name: string, args = "") => {
      if (!sessionId) return undefined;
      return withErrors(async () => {
        await ensureLive();
        return api.command(sessionId, name, args);
      });
    },
    [sessionId, withErrors, ensureLive],
  );

  const switchAgent = useCallback(
    async (id: string) => {
      if (!sessionId) {
        setDraft({ agent: id, model: null, variant: undefined });
        return;
      }
      if (id === agentId) return;
      await command("agent", id);
    },
    [sessionId, agentId, command],
  );

  const variantsOf = useCallback((name: string | null) => chatModels.find((m) => m.name === name)?.variants ?? [], [chatModels]);

  /** Moves the active agent to `name`, keeping the variant when the new model offers it. */
  const pickModel = useCallback(
    async (name: string) => {
      const keep = selection.variant;
      const offered = variantsOf(name);
      if (!sessionId) {
        setDraft((d) => ({ ...d, agent: d.agent ?? agentId, model: name, variant: keep && offered.includes(keep) ? keep : undefined }));
        return;
      }
      if (name === selection.model) return;
      const result = await command("model", name);
      if (result?.ok && result.data?.applies === "now" && keep && offered.includes(keep) && result.data?.variant !== keep) await command("variant", keep);
    },
    [sessionId, selection, variantsOf, command, agentId],
  );

  const pickVariant = useCallback(
    async (variant: string | null) => {
      if (!sessionId) {
        setDraft((d) => ({ ...d, agent: d.agent ?? agentId, model: d.model ?? selection.model, variant }));
        return;
      }
      if (variant === selection.variant) return;
      await command("variant", variant ?? "off");
    },
    [sessionId, selection, command, agentId],
  );

  /** Puts the active agent back on its configured model and variant. */
  const backToConfig = useCallback(async () => {
    const assigned = agentEntry?.assignment;
    if (!sessionId) {
      setDraft((d) => ({ ...d, model: null, variant: undefined }));
      return;
    }
    if (!assigned) return;
    if (assigned.model !== selection.model) {
      const result = await command("model", assigned.model);
      if (!result?.ok) return;
    }
    const word = assigned.variant_source === "model_default" ? "inherit" : assigned.variant ?? "off";
    await command("variant", word);
  }, [agentEntry, sessionId, selection.model, command]);

  const answerPermission = useCallback(
    async (requestId: string, decision: "allow_once" | "allow_folder" | "deny") => {
      if (!sessionId) return;
      try {
        await api.answerPermission(sessionId, requestId, decision);
      } catch (e) {
        if (e instanceof HttpError && e.reason === "permission_stale") throw new Error("This request is no longer waiting: it was answered, cancelled or expired.");
        throw e;
      }
    },
    [sessionId],
  );

  return {
    summary: summary.data ?? null,
    summaryError: summary.error,
    stream,
    view,
    agents: agents.data ?? null,
    chatModels,
    defaultAgent,
    agentId,
    agentEntry,
    selection,
    draft,
    readOnly,
    stopped,
    running,
    busy,
    error,
    setError,
    send,
    stop,
    command,
    switchAgent,
    pickModel,
    pickVariant,
    backToConfig,
    variantsOf,
    answerPermission,
  };
}

export type Chat = ReturnType<typeof useChat>;
