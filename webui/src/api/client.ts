import { blob, del, get, HttpError, post, put, q, request } from "./http";
import type { AskAnswer, SecretScope, SecretTtl } from "../session/reducer";
import type {
  AgentsBody,
  CommandInfo,
  CommandOutcome,
  ContentBlock,
  ContextSnapshot,
  GatewayTest,
  ImageRef,
  IntentBody,
  LayerEntry,
  MembershipChange,
  MemoryFilter,
  MemoryGroup,
  MemoryItem,
  MemoryItemDetail,
  MemoryProject,
  McpResourceBody,
  McpStatus,
  ModelEntry,
  ProjectsBody,
  SessionLedger,
  SessionListEntry,
  SessionSummary,
  SecretView,
  SettingsBody,
  SkillsBody,
  WaitingEntry,
  WorkspaceEntry,
} from "./types";

export type PermissionDecision = "allow_once" | "allow_always" | "reject" | "hold";
export interface PermissionBinding {
  callDigest?: string;
  turnId?: string;
}

/**
 * What an answer's refusal tells the Captain. A request answered elsewhere is stale; a call the
 * review already allowed is never described as undone.
 */
export function permissionAnswerError(error: unknown): Error {
  if (!(error instanceof HttpError) || error.status !== 409) return error instanceof Error ? error : new Error(String(error));
  if (error.reason === "permission_stale") return new Error("This request is no longer waiting: it was answered, cancelled or expired.");
  if (error.reason === "permission_already_started") return new Error("Already started by Metacognition — it ran once.");
  return error;
}

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
  ledger: (id: string) => get<SessionLedger>(`/v1/sessions/${q(id)}/ledger`),
  context: (id: string) => get<ContextSnapshot>(`/v1/sessions/${q(id)}/context`),
  layers: (workspace: string) => get<{ layers: LayerEntry[] }>(`/v1/layers?workspace=${q(workspace)}`).then((b) => b.layers ?? []),
  /** The service's MCP servers; the live `mcp.status` event carries the same body. */
  mcp: () => get<McpStatus>("/v1/mcp"),
  mcpResource: (server: string, uri: string) =>
    get<McpResourceBody>(`/v1/mcp/${q(server)}/resource?uri=${q(uri)}`),
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  mcpCall: (server: string, name: string, args: Record<string, any> = {}) =>
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    post<any>(`/v1/mcp/${q(server)}/call`, { name, arguments: args }),
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
  /** Store an image for the session; the server checks its type from the bytes. */
  upload: (id: string, file: File) =>
    request<ImageRef>(`/v1/sessions/${q(id)}/attachments?name=${q(file.name || "image")}`, {
      method: "POST",
      headers: { "content-type": file.type || "application/octet-stream" },
      body: file,
    }),
  attachment: (id: string, attachmentId: string) => blob(`/v1/sessions/${q(id)}/attachments/${q(attachmentId)}`),
  stop: (id: string) => post<{ accepted: boolean; actor_state: string }>(`/v1/sessions/${q(id)}/stop`),
  answerQuestion: (id: string, answer: AskAnswer) =>
    post<{ accepted: boolean }>(`/v1/sessions/${q(id)}/messages`, { target_actor_id: "agent:main", mode: "next-turn", content: [], answer }),
  command: (id: string, name: string, args = "") =>
    post<CommandOutcome>(`/v1/sessions/${q(id)}/commands`, { name, arguments: args }),
  continueChild: (parent: string, child: string, token?: { incarnation: string; hold_id: string }) =>
    post<{ accepted: boolean }>(`/v1/sessions/${q(parent)}/children/${q(child)}/continue`, { pause_token: token ?? null }),
  /** The Captain's secret for `requestId`: the only request that carries a value. */
  submitSecret: (id: string, requestId: string, body: { name?: string; value: string; ttl: SecretTtl; scope: SecretScope }) =>
    post<{ accepted: boolean; secret?: SecretView }>(`/v1/sessions/${q(id)}/secrets/${q(requestId)}`, body),
  declineSecret: (id: string, requestId: string) =>
    post<{ accepted: boolean }>(`/v1/sessions/${q(id)}/secrets/${q(requestId)}`, { decline: true }),
  secrets: (id: string) => get<{ secrets: SecretView[] }>(`/v1/sessions/${q(id)}/secrets`).then((b) => b.secrets ?? []),
  revokeSecret: (id: string, name: string) => del<void>(`/v1/sessions/${q(id)}/secrets/${q(name)}`),
  /**
   * `binding` ties the answer to the `permission.requested` it was given for: another call under
   * the same request, or another turn, is refused as `permission_stale`. `hold` is "Hold for
   * me": it stops the parallel review and keeps the card.
   */
  answerPermission: (id: string, requestId: string, decision: PermissionDecision, message?: string, binding?: PermissionBinding) =>
    post<{ accepted: boolean; held?: boolean }>(`/v1/sessions/${q(id)}/permissions/${q(requestId)}`, {
      decision,
      ...(message ? { message } : {}),
      ...(binding?.callDigest ? { call_digest: binding.callDigest } : {}),
      ...(binding?.turnId ? { turn_id: binding.turnId } : {}),
    }),
  /** The permission requests every live session waits on, oldest first. */
  waiting: () => get<{ waiting: WaitingEntry[] }>("/v1/waiting").then((b) => b.waiting ?? []),

  // ---- memory register ----
  memoryItems: (filter: MemoryFilter = {}) => {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(filter)) if (value !== undefined && value !== "") params.set(key, String(value));
    const query = params.toString();
    return get<{ items: MemoryItem[] }>(`/v1/memory${query ? `?${query}` : ""}`).then((b) => b.items ?? []);
  },
  memoryItem: (id: string) => get<MemoryItemDetail>(`/v1/memory/${q(id)}`),
  reviseMemory: (id: string, body: { expected_version: number; text?: string; title?: string; why?: string; reason?: string }) =>
    post<MemoryItem>(`/v1/memory/${q(id)}/revise`, body),
  invalidateMemory: (id: string, body: { expected_version: number; reason?: string }) => post<MemoryItem>(`/v1/memory/${q(id)}/invalidate`, body),
  restoreMemory: (id: string, body: { expected_version: number; from_version?: number; reason?: string }) =>
    post<MemoryItem>(`/v1/memory/${q(id)}/restore`, body),
  rescopeMemory: (id: string, body: { expected_version: number; scope: string; scope_ref?: string; confirm?: boolean; reason?: string }) =>
    post<MemoryItem>(`/v1/memory/${q(id)}/rescope`, body),
  needsYou: () => get<{ changes: MembershipChange[] }>("/v1/memory/needs-you").then((b) => b.changes ?? []),
  decideChange: (id: number, decision: "accept" | "reject") => post<MembershipChange>(`/v1/memory/needs-you/${id}/${decision}`),
  projects: () => get<ProjectsBody>("/v1/projects"),
  createGroup: (name: string) => post<MemoryGroup>("/v1/groups", { name }),
  renameGroup: (id: string, name: string) => request<MemoryGroup>(`/v1/groups/${q(id)}`, { method: "PATCH", body: JSON.stringify({ name }) }),
  /** `confirm: false` is refused with the affected projects when the addition widens who reads the group's items. */
  addProjectToGroup: (project: string, group: string, confirm: boolean) =>
    put<MembershipChange>(`/v1/projects/${q(project)}/groups/${q(group)}`, { confirm }),
  removeProjectFromGroup: (project: string, group: string) => del<MembershipChange>(`/v1/projects/${q(project)}/groups/${q(group)}`),
  /** A project id attaches the session; `null` detaches it. The link stays in history either way. */
  sessionProject: (session: string, project: string | null) => put<MembershipChange>(`/v1/sessions/${q(session)}/project`, { project_id: project }),
  answerGroup: (project: string, body: { group_id: string | null; later?: boolean }) => post<MemoryProject>(`/v1/projects/${q(project)}/group-answer`, body),
};
