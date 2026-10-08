import { Crew } from "./Crew";
import { ArrowElbowDownRight, Hammer, Notebook, Robot } from "@phosphor-icons/react";
import { Fragment } from "react";
import { useIntent, useSkills } from "../api/queries";
import type { AgentEntry } from "../api/types";
import type { Child } from "../session/reducer";
import type { Chat } from "../session/useChat";

export type PanelTab = "intent" | "agents" | "extensions";
const TABS: [PanelTab, string][] = [
  ["intent", "Intent"],
  ["agents", "Agents"],
  ["extensions", "Extensions"],
];

export function RightPanel({ tab, setTab, workspace, chat, onOpenChild }: { tab: PanelTab; setTab: (t: PanelTab) => void; workspace: string | null; chat: Chat; onOpenChild: (id: string) => void }) {
  return (
    <aside className="panel" aria-label="Session details">
      <div className="panel-tabs" role="tablist">
        {TABS.map(([id, label]) => (
          <button key={id} type="button" role="tab" aria-selected={tab === id} className="panel-tab" onClick={() => setTab(id)}>
            {label}
          </button>
        ))}
      </div>
      <div className="panel-body" role="tabpanel">
        {tab === "intent" && <IntentTab workspace={workspace} />}
        {tab === "agents" && <AgentsTab chat={chat} onOpenChild={onOpenChild} />}
        {tab === "extensions" && <ExtensionsTab chat={chat} />}
      </div>
    </aside>
  );
}

function IntentTab({ workspace }: { workspace: string | null }) {
  const intent = useIntent(workspace);
  const label = <div className="label">INTENT RECORD · .withnav/intent.md</div>;
  if (!workspace) return <>{label}<div className="panel-empty">Choose a workspace to see its intent record.</div></>;
  if (intent.isError) return <>{label}<div className="panel-empty">The intent record is not available from this server.</div></>;
  if (!intent.data) return label;
  if (!intent.data.exists)
    return (
      <>
        {label}
        <div className="panel-empty">
          No intent record in this workspace yet. In an editor session, Nav writes one when an intent, a plan or a decision is settled. You can also create <span className="mono">.withnav/intent.md</span> by hand.
        </div>
      </>
    );
  const sections: [string, string | null][] = [
    ["intent", intent.data.intent],
    ["plan", intent.data.plan],
    ["decisions", intent.data.decisions],
    ["active spec", intent.data.active_spec],
  ];
  const present = sections.filter(([, text]) => text);
  return (
    <>
      {label}
      <div className="intent">
        {present.length
          ? present.map(([name, text]) => (
              <Fragment key={name}>
                <span className="h">## {name}</span>
                {"\n"}
                {text}
                {"\n"}
              </Fragment>
            ))
          : intent.data.raw}
      </div>
    </>
  );
}

const ICON: Record<string, typeof Hammer> = { build: Hammer, plan: Notebook };

function AgentsTab({ chat, onOpenChild }: { chat: Chat; onOpenChild: (id: string) => void }) {
  const agents = chat.agents?.agents ?? [];
  if (!chat.agents) return <div className="panel-empty">Loading agents…</div>;
  const primaries = agents.filter((a) => a.mode === "primary" || a.mode === "all");
  const subagents = agents.filter((a) => a.mode === "subagent");
  const children = Object.values(chat.view.children);
  const childrenOf = (agent: string) => children.filter((c) => c.agent === agent);
  return (
    <div className="cards">
      {primaries.map((a) => (
        <AgentCard key={a.id} a={a} active={a.id === chat.agentId} isDefault={a.id === chat.defaultAgent} chat={chat} />
      ))}
      <div className="label" style={{ marginTop: 6 }}>SUBAGENTS</div>
      {subagents.map((a) => (
        <Fragment key={a.id}>
          <AgentCard a={a} sub chat={chat} running={childrenOf(a.id).filter((c) => c.status === "running").length} />
          {childrenOf(a.id).map((c) => (
            <ChildCard key={c.id} c={c} onOpen={() => onOpenChild(c.id)} />
          ))}
        </Fragment>
      ))}
      {!subagents.length && <div className="panel-empty">No subagent is defined.</div>}
      <div className="label" style={{ marginTop: 4 }}>AGENT FILES · .withnav/agents/*.md</div>
    </div>
  );
}

function modelLine(a: AgentEntry): string {
  if (a.inherits_parent) return "model: same as the agent that starts it";
  if (!a.effective) return "no usable model";
  return `${a.effective.model}${a.effective.variant ? ` · ${a.effective.variant}` : ""}${a.effective.source === "override" ? " · session override" : ""}`;
}

function AgentCard({ a, active, isDefault, sub, running, chat }: { a: AgentEntry; active?: boolean; isDefault?: boolean; sub?: boolean; running?: number; chat: Chat }) {
  const Icon = sub ? ArrowElbowDownRight : ICON[a.id] ?? Robot;
  const live = active ? { model: chat.selection.model, variant: chat.selection.variant } : null;
  return (
    <div className={`card${sub ? " sub" : ""}${active ? " active" : ""}${a.id === "plan" ? " plan" : ""}`}>
      <div className="head">
        <Icon size={14} style={{ color: active ? (a.id === "plan" ? "var(--bl)" : "var(--ac)") : undefined }} aria-hidden />
        <Crew id={a.id} />
        <span className={sub && running ? "" : "muted"} style={sub && running ? { color: "var(--pu)" } : undefined}>
          · {sub ? `subagent${running ? ` · ${running} running` : ""}` : `primary${active ? " · active" : ""}${isDefault ? " · default" : ""}`}
        </span>
      </div>
      {a.description && <div className="desc">{a.description}</div>}
      <div className="model" title={modelLine(a)}>
        {live?.model ? `${live.model}${live.variant ? ` · ${live.variant}` : ""}` : modelLine(a)}
      </div>
      {a.warning && <div className="warn">{a.warning}</div>}
    </div>
  );
}

function ChildCard({ c, onOpen }: { c: Child; onOpen: () => void }) {
  return (
    <button type="button" className="card sub" style={{ marginLeft: 36, textAlign: "left", background: "transparent", color: "inherit" }} onClick={onOpen}>
      <div className="head">
        <span style={{ color: "var(--pu)" }}><Crew id={c.agent} /></span>
        <span className="muted">· {c.status} · spawned by {c.spawnedBy}</span>
      </div>
      <div className="desc" style={{ display: "-webkit-box", WebkitLineClamp: 2, WebkitBoxOrient: "vertical", overflow: "hidden" }}>{c.goal}</div>
    </button>
  );
}

function ExtensionsTab({ chat }: { chat: Chat }) {
  const id = chat.summary?.session_id ?? null;
  const live = !!chat.summary && chat.summary.actor_state !== "stopped";
  const skills = useSkills(id, live);
  return (
    <>
      <div className="section-label">SKILLS{skills.data?.skills.length ? ` · ${skills.data.skills.length}` : ""}</div>
      {!id && <div className="panel-empty">Skills are listed once the session has started.</div>}
      {id && !live && <div className="panel-empty">Skills are listed while the session runs. Send a message to resume it.</div>}
      {skills.isError && <div className="panel-empty">Skills could not be loaded.</div>}
      {skills.data?.disabled_reason && <div className="panel-empty">{skills.data.disabled_reason}</div>}
      {skills.data && !skills.data.disabled_reason && !skills.data.skills.length && <div className="panel-empty">No skill is available to this session.</div>}
      {!!skills.data?.skills.length && (
        <div className="ext-list">
          {skills.data.skills.map((s) => (
            <div key={s.name} className="ext">
              <span className="mono">{s.name}</span>
              {s.description && <div className="muted clamp" style={{ fontSize: 11.5 }} title={s.description}>{s.description}</div>}
            </div>
          ))}
        </div>
      )}
      <div className="section-label">MCP SERVERS</div>
      <div className="panel-empty">The server does not report MCP servers yet.</div>
      <div className="section-label">PLUGINS · JSON-RPC over stdio</div>
      <div className="panel-empty">The server does not report plugins yet.</div>
    </>
  );
}
