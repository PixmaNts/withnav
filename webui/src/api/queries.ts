import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { api } from "./client";
import type { McpStatus } from "./types";

// Query keys in one place, so mutations invalidate exactly what they change.
export const keys = {
  workspaces: ["workspaces"] as const,
  sessions: (ws: string) => ["sessions", ws] as const,
  session: (id: string) => ["session", id] as const,
  models: (ws: string) => ["models", ws] as const,
  agents: (ws: string, id: string | null) => ["agents", ws, id] as const,
  commands: ["commands"] as const,
  skills: (id: string) => ["skills", id] as const,
  intent: (ws: string) => ["intent", ws] as const,
  context: (id: string) => ["context", id] as const,
  settings: (ws: string) => ["settings", ws] as const,
  layers: (ws: string) => ["layers", ws] as const,
  mcp: ["mcp"] as const,
};

/** Everything a workspace removal or reopening changes: the list and every session list. */
export const workspacesChanged = (qc: QueryClient) =>
  Promise.all([qc.invalidateQueries({ queryKey: keys.workspaces }), qc.invalidateQueries({ queryKey: ["sessions"] })]);

/** After a session deletion: its cached summary goes, and every list and count is fetched again. */
export const sessionsChanged = (qc: QueryClient, deleted: string[]) => {
  for (const id of deleted) qc.removeQueries({ queryKey: keys.session(id), exact: true });
  return workspacesChanged(qc);
};

export const useWorkspaces = () => useQuery({ queryKey: keys.workspaces, queryFn: api.workspaces, refetchInterval: 30_000 });

export const useSessions = (ws: string | null) =>
  useQuery({ queryKey: keys.sessions(ws ?? ""), queryFn: () => api.sessions(ws!), enabled: !!ws, refetchInterval: 15_000 });

export const useSessionSummary = (id: string | null) =>
  useQuery({ queryKey: keys.session(id ?? ""), queryFn: () => api.session(id!), enabled: !!id });

export const useModels = (ws: string | null) =>
  useQuery({ queryKey: keys.models(ws ?? ""), queryFn: () => api.models(ws!), enabled: !!ws, staleTime: 5 * 60_000 });

export const useAgents = (ws: string | null, id: string | null) =>
  useQuery({ queryKey: keys.agents(ws ?? "", id), queryFn: () => api.agents(ws!, id ?? undefined), enabled: !!ws });

export const useCommands = () => useQuery({ queryKey: keys.commands, queryFn: api.commands, staleTime: Infinity });

export const useSkills = (id: string | null, live: boolean) =>
  useQuery({ queryKey: keys.skills(id ?? ""), queryFn: () => api.skills(id!), enabled: !!id && live, retry: false });

export const useIntent = (ws: string | null) =>
  useQuery({ queryKey: keys.intent(ws ?? ""), queryFn: () => api.intent(ws!), enabled: !!ws, refetchInterval: 20_000, retry: false });

/** The context gauge of a live session; `context.updated` is live-only, so a reopened stream lacks it. */
export const useContextSnapshot = (id: string | null, live: boolean) =>
  useQuery({ queryKey: keys.context(id ?? ""), queryFn: () => api.context(id!), enabled: !!id && live, retry: false });

export const useSettings = (ws: string | null) =>
  useQuery({ queryKey: keys.settings(ws ?? ""), queryFn: () => api.settings(ws!), enabled: !!ws, retry: false });
export const useLayers = (ws: string | null) =>
  useQuery({ queryKey: keys.layers(ws ?? ""), queryFn: () => api.layers(ws!), enabled: !!ws, retry: false });

/** True when `next` may replace `held`: another service incarnation, or a revision not older. */
export const newerMcp = (held: McpStatus | undefined, next: McpStatus): boolean =>
  !held || held.incarnation !== next.incarnation || next.revision >= held.revision;

/**
 * The service's MCP servers. Read at start and every 30 s; an open conversation's stream also
 * brings each change as `mcp.status` (see {@link keepMcpStatus}), and a read never replaces a
 * newer event.
 */
export const useMcp = () => {
  const qc = useQueryClient();
  return useQuery({
    queryKey: keys.mcp,
    queryFn: async () => {
      const next = await api.mcp();
      const held = qc.getQueryData<McpStatus>(keys.mcp);
      return held && !newerMcp(held, next) ? held : next;
    },
    refetchInterval: 30_000,
    retry: false,
  });
};

/** Applies an `mcp.status` payload to the cache, unless the one held is newer. */
export const keepMcpStatus = (qc: QueryClient, next: McpStatus) =>
  qc.setQueryData<McpStatus>(keys.mcp, (held) => (newerMcp(held, next) ? next : held));
