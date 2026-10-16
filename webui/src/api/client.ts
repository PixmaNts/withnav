import { del, get, post, put, q } from "./http";
import type { AskAnswer } from "../session/reducer";
import type {
  AgentsBody,
  CommandInfo,
  CommandOutcome,
  ContentBlock,
  ContextSnapshot,
  GatewayTest,
  IntentBody,
  LayerEntry,
  McpStatus,
  ModelEntry,
  SessionListEntry,
  SessionSummary,
  SettingsBody,
  SkillsBody,
  WorkspaceEntry,
} from "./types";

export const api = {
  workspaces: () => get<{ workspaces: WorkspaceEntry[] }>("/v1/workspaces").then((b) => b.workspaces ?? []),
  /** Hide a folder from the list; its sessions are kept. */
  hideWorkspace: (path: string) => del<void>(`/v1/workspaces?path=${q(path)}`),
  /** Delete every session of a folder (with their subagents), then hide it. */
  deleteWorkspaceSessions: (path: string) =>
    del<{ deleted: number }>(`/v1/workspaces?path=${q(path)}&sessions=delete`),
  /** Show a hidden folder again; harmless on one that is not hidden. */
  unhideWorkspace: (path: string) => post<void>(`/v1/workspaces?path=${q(path)}`),
  sessions: (workspace: string) =>
    get<{ sessions: SessionListEntry[] }>(`/v1/sessions?workspace=${q(workspace)}`).then((b) => b.sessions ?? []),
  session: (id: string) => get<SessionSummary>(`/v1/sessions/${q(id)}`),
  /** Delete a session for good, with its subagent sessions; a running one is stopped first. 404 when already gone. */
  remove: (id: string) => del<void>(`/v1/sessions/${q(id)}`),
  setBypass: (id: string, enabled: boolean) => post<{ enabled: boolean; changed: boolean; bypass_seq: number }>(`/v1/sessions/${q(id)}/bypass`, { enabled }),
  models: (workspace: string) =>
    get<{ models: ModelEntry[]; default_agent: string }>(`/v1/models?workspace=${q(workspace)}`),
  agents: (workspace: string, sessionId?: string) =>
    get<AgentsBody>(`/v1/agents?workspace=${q(workspace)}${sessionId ? `&session_id=${q(sessionId)}` : ""}`),
  commands: () => get<{ commands: CommandInfo[] }>("/v1/commands").then((b) => b.commands ?? []),
  skills: (id: string) => get<SkillsBody>(`/v1/sessions/${q(id)}/skills`),
  context: (id: string) => get<ContextSnapshot>(`/v1/sessions/${q(id)}/context`),
  layers: (workspace: string) => get<{ layers: LayerEntry[] }>(`/v1/layers?workspace=${q(workspace)}`).then((b) => b.layers ?? []),
  /** The service's MCP servers; the live `mcp.status` event carries the same body. */
  mcp: () => get<McpStatus>("/v1/mcp"),
  settings: (workspace: string) => get<SettingsBody>(`/v1/settings?workspace=${q(workspace)}`),
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  saveSetting: (workspace: string, body: { revision: string; key: string; value: any }) =>
    put<SettingsBody>(`/v1/settings?workspace=${q(workspace)}`, body),
  testGateway: (workspace: string, name: string) =>
    post<GatewayTest>(`/v1/gateways/${q(name)}/test?workspace=${q(workspace)}`),
  intent: (workspace: string) => get<IntentBody>(`/v1/intent?workspace=${q(workspace)}`),

  create: (body: { workspace: string; agent?: string; model?: string; variant?: string | null; resume?: string }) =>
    post<{ session_id: string; actor_state: string }>("/v1/sessions", body),
  send: (id: string, content: ContentBlock[]) =>
    post<{ command_id: string; accepted: boolean; actor_state: string; notice?: string }>(`/v1/sessions/${q(id)}/messages`, {
      target_actor_id: "agent:main",
      mode: "next-turn",
      content,
    }),
  stop: (id: string) => post<{ accepted: boolean; actor_state: string }>(`/v1/sessions/${q(id)}/stop`),
  answerQuestion: (id: string, answer: AskAnswer) =>
    post<{ accepted: boolean }>(`/v1/sessions/${q(id)}/messages`, { target_actor_id: "agent:main", mode: "next-turn", content: [], answer }),
  command: (id: string, name: string, args = "") =>
    post<CommandOutcome>(`/v1/sessions/${q(id)}/commands`, { name, arguments: args }),
  continueChild: (parent: string, child: string, token?: { incarnation: string; hold_id: string }) =>
    post<{ accepted: boolean }>(`/v1/sessions/${q(parent)}/children/${q(child)}/continue`, { pause_token: token ?? null }),
  answerPermission: (id: string, requestId: string, decision: "allow_once" | "allow_always" | "reject", message?: string) =>
    post<{ accepted: boolean }>(`/v1/sessions/${q(id)}/permissions/${q(requestId)}`, message ? { decision, message } : { decision }),
};
