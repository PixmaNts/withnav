import { Compass, WifiSlash } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api/client";
import { onUnauthorized } from "./api/http";
import { keys, useCommands, useSessions } from "./api/queries";
import { CommandPalette, type PaletteCommand, type PalettePage } from "./components/CommandPalette";
import { Composer, type ComposerHandle } from "./components/Composer";
import { ModelPicker } from "./components/ModelPicker";
import { RightPanel, type PanelTab } from "./components/RightPanel";
import { Sidebar } from "./components/Sidebar";
import { SubagentView } from "./components/SubagentView";
import { EmptySession, Transcript } from "./components/Transcript";
import type { ClientCommand } from "./lib/compose";
import { baseName, modKey, shortPath } from "./lib/format";
import { useRoute } from "./router";
import { useChat } from "./session/useChat";
import { applyTheme, storedTheme, THEMES, type ThemeId } from "./theme/themes";

type Overlay = null | "models" | { palette: PalettePage };

export function App() {
  const [expired, setExpired] = useState(false);
  useEffect(() => onUnauthorized(() => setExpired(true)), []);
  if (expired) return <Expired />;
  return <ChatPage />;
}

function Expired() {
  return (
    <div className="fullpage">
      <div className="box" role="alert">
        <h1>
          <Compass size={18} aria-hidden /> Session expired
        </h1>
        <div>
          The Navai server no longer accepts this page. Relaunch <code>navai web</code> and open the address it prints.
        </div>
      </div>
    </div>
  );
}

function ChatPage() {
  const qc = useQueryClient();
  const [route, navigate] = useRoute();
  const [theme, setTheme] = useState<ThemeId>(storedTheme);
  const [overlay, setOverlay] = useState<Overlay>(null);
  const [tab, setTab] = useState<PanelTab>("intent");
  const composer = useRef<ComposerHandle>(null);
  const workspace = route.workspace;
  const onCreated = useCallback((id: string) => navigate({ session: id, subagent: null }, true), [navigate]);
  const chat = useChat(workspace, route.session, onCreated);
  const sessions = useSessions(workspace);
  const commands = useCommands();
  const sessionRow = sessions.data?.find((s) => s.id === route.session);
  const title = sessionRow?.title || (route.session ? "Untitled session" : "New session");
  const childRow = route.subagent ? sessions.data?.find((s) => s.id === route.subagent) : undefined;
  const childAgent = (route.subagent && chat.view.children[route.subagent]?.agent) || childRow?.agent;

  useEffect(() => applyTheme(theme), [theme]);
  useEffect(() => {
    document.title = route.session ? `${title} · Navai` : "Navai";
  }, [route.session, title]);

  // A route whose workspace the stream reports differently is corrected.
  useEffect(() => {
    const ws = chat.summary?.workspace;
    if (ws && ws !== workspace) navigate({ workspace: ws }, true);
  }, [chat.summary?.workspace, workspace, navigate]);

  const close = useCallback(() => {
    setOverlay(null);
    requestAnimationFrame(() => composer.current?.focus());
  }, []);

  const otherAgent = chat.agentId === "plan" ? "build" : "plan";
  const toggleAgent = useCallback(() => void chat.switchAgent(chat.agentId === "plan" ? "build" : "plan"), [chat]);
  const openChild = useCallback((id: string) => navigate({ subagent: id }), [navigate]);
  const newSession = useCallback(() => navigate({ session: null, subagent: null }), [navigate]);

  const themePage = useCallback(
    (): PalettePage => ({
      title: "Theme",
      placeholder: "Choose a theme…",
      items: THEMES.map((t) => ({ id: t.id, group: "Theme", label: t.label, hint: t.id === theme ? "current" : "", run: () => setTheme(t.id) })),
    }),
    [theme],
  );

  const skillsPage = useCallback(
    (): PalettePage => ({ title: "Skills", placeholder: "Choose a skill…", items: [], empty: "Loading…" }),
    [],
  );

  const palette = useMemo((): PalettePage => {
    const hasSession = !!route.session;
    const children = Object.values(chat.view.children);
    const items: PaletteCommand[] = [
      { id: "agent", group: "Agent", label: `Switch agent to ${otherAgent}`, hint: "tab", run: toggleAgent, disabled: workspace ? undefined : "Choose a workspace first" },
      { id: "model", group: "Agent", label: "Model for this session…", hint: "/model", run: () => setOverlay("models"), disabled: workspace ? undefined : "Choose a workspace first" },
      {
        id: "subagent",
        group: "Agent",
        label: "Open subagent…",
        hint: "",
        disabled: children.length ? undefined : "No subagent in this session",
        page: () => ({
          title: "Subagents",
          placeholder: "Choose a subagent…",
          items: children.map((c) => ({ id: c.id, group: c.status, label: `@${c.agent}`, description: c.goal.slice(0, 80), run: () => openChild(c.id) })),
        }),
      },
      { id: "delegate", group: "Agent", label: "Delegate to cognition", hint: "", run: () => composer.current?.toggleDelegate(), disabled: workspace ? undefined : "Choose a workspace first" },
      { id: "new", group: "Session", label: "New session", hint: "/new", run: newSession, disabled: workspace ? undefined : "Choose a workspace first" },
      {
        id: "open",
        group: "Session",
        label: "Open session…",
        hint: "",
        disabled: sessions.data?.length ? undefined : "No session in this workspace",
        page: () => ({
          title: "Sessions",
          placeholder: "Choose a session…",
          items: (sessions.data ?? []).filter((s) => !s.parent_session_id).map((s) => ({ id: s.id, group: s.agent, label: s.title || "Untitled session", description: `${s.message_count} msgs`, run: () => navigate({ session: s.id, subagent: null }) })),
        }),
      },
      ...(hasSession && chat.running ? [{ id: "stop", group: "Session", label: "Stop the session", hint: "/stop", run: () => void chat.stop() }] : []),
      {
        id: "skill",
        group: "Extensions",
        label: "Run skill…",
        hint: "/skill",
        disabled: hasSession ? undefined : "Skills are listed once the session has started",
        page: skillsPage,
      },
      { id: "theme", group: "View", label: "Theme…", hint: "", page: themePage },
    ];
    // The server's commands, minus the ones above already cover.
    const covered = new Set(["model", "new", "agent", "stop"]);
    for (const c of commands.data ?? []) {
      if (covered.has(c.name)) continue;
      items.push({
        id: `server:${c.name}`,
        group: "Session",
        label: c.description,
        hint: `/${c.name}`,
        disabled: hasSession ? undefined : "Send a first message to start the session",
        run: () => (c.args_hint.startsWith("<") ? composer.current?.fill(`/${c.name} `) : void runServer(c.name, "")),
      });
    }
    const order = ["Agent", "Session", "Extensions", "View"];
    items.sort((a, b) => order.indexOf(a.group) - order.indexOf(b.group));
    return { title: "Commands", placeholder: "Type a command or search…", items };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [route.session, chat.view.children, chat.running, otherAgent, workspace, sessions.data, commands.data, themePage, skillsPage]);

  const runServer = async (name: string, args: string) => {
    if (!route.session) {
      chat.setError("Send a first message to start the session, then run commands.");
      return;
    }
    await chat.command(name, args);
  };

  const clientCommand = (name: ClientCommand, args: string) => {
    if (name === "model") setOverlay("models");
    else if (name === "theme") setOverlay({ palette: themePage() });
    else if (name === "skills") openSkills();
    else if (name === "new") {
      newSession();
      if (args) void chat.switchAgent(args);
    }
  };

  const openSkills = useCallback(() => {
    if (!route.session) {
      chat.setError("Skills are listed once the session has started.");
      return;
    }
    void qc
      .fetchQuery({ queryKey: keys.skills(route.session), queryFn: () => api.skills(route.session!) })
      .then((body) =>
        setOverlay({
          palette: {
            title: "Skills",
            placeholder: "Choose a skill…",
            empty: body.disabled_reason ?? "No skill is available to this session.",
            items: body.disabled_reason ? [] : body.skills.map((s) => ({ id: s.name, group: "Skill", label: s.name, description: s.description, run: () => composer.current?.fill(`/skill ${s.name} `) })),
          },
        }),
      )
      .catch((e) => chat.setError(e instanceof Error ? e.message : String(e)));
  }, [route.session, qc, chat]);

  // The skills sub-page of the palette is filled from the server on demand.
  const paletteWithSkills = useMemo((): PalettePage => {
    return {
      ...palette,
      items: palette.items.map((item) => (item.id === "skill" && !item.disabled ? { ...item, page: undefined, run: openSkills } : item)),
    };
  }, [palette, openSkills]);

  // ⌘K / Ctrl+K anywhere; Escape closes whatever is open.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOverlay((o) => (o && typeof o === "object" ? null : { palette: paletteWithSkills }));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [paletteWithSkills]);

  // No variant reads "off" where the model offers some (the word `/variant` takes), else nothing.
  const offered = chat.variantsOf(chat.selection.model);
  const variantLabel = chat.selection.variant ?? (offered.length ? "off" : "");
  const modelLabel = chat.selection.model ? `${chat.selection.model}${variantLabel ? ` · ${variantLabel}` : ""}` : "no model";
  const connection = chat.stream.status === "reconnecting" ? "Reconnecting…" : chat.stream.status === "error" ? chat.stream.error ?? "Disconnected" : null;
  const composerDisabled = !workspace ? "Choose a workspace on the left to start." : chat.readOnly ? chat.summary?.resume_disabled_reason ?? "This session is read-only." : undefined;

  const crumbWs = workspace ? baseName(workspace) : "Navai";
  const crumb = route.subagent ? `${title} / ${childAgent ?? "subagent"}` : workspace ? title : "Choose a workspace";

  return (
    <div className="shell">
      <header className="topbar">
        <span className="brand">
          <Compass size={16} aria-hidden />
          Navai
        </span>
        <span className="crumb" title={workspace ?? undefined}>
          {crumbWs} / {crumb}
        </span>
        {connection && (
          <span className="conn" role="status">
            <WifiSlash size={13} aria-hidden /> {connection}
          </span>
        )}
        <div className="topbar-right">
          <span className="muted" style={{ fontSize: 12 }}>
            Agent
          </span>
          {["build", "plan"].map((id) => (
            <button
              key={id}
              type="button"
              className={`seg ${id}`}
              aria-pressed={chat.agentId === id}
              disabled={!workspace || chat.readOnly}
              onClick={() => void chat.switchAgent(id)}
              title={chat.running ? "Switches when the current turn ends" : `Run this session as ${id}`}
            >
              {id}
            </button>
          ))}
          <button type="button" className="kbd" onClick={() => setOverlay({ palette: paletteWithSkills })} aria-label="Open the command palette" title="Command palette">
            {modKey("K")}
          </button>
        </div>
      </header>

      <Sidebar route={route} navigate={navigate} />

      {route.subagent && route.session ? (
        <main className="main">
          <SubagentView childId={route.subagent} parentId={route.session} parent={chat.view} parentTitle={title} onBack={() => navigate({ subagent: null })} />
        </main>
      ) : (
        <main className="main">
          <Transcript
            view={chat.view}
            agent={chat.summary?.agent}
            onAnswer={chat.answerPermission}
            onOpenChild={openChild}
            readOnly={chat.readOnly}
            empty={<EmptyState workspace={workspace} sessionId={route.session} loading={chat.stream.status === "connecting"} agent={chat.agentId} />}
          />
          <Composer
            ref={composer}
            agent={chat.agentId}
            modelLabel={modelLabel}
            pickerOpen={overlay === "models"}
            running={chat.running}
            disabled={composerDisabled}
            context={chat.view.context}
            serverCommands={commands.data ?? []}
            error={chat.error}
            onOpenModels={() => setOverlay(overlay === "models" ? null : "models")}
            onToggleAgent={toggleAgent}
            onSend={chat.send}
            onServerCommand={runServer}
            onClientCommand={clientCommand}
            onStop={() => void chat.stop()}
            onError={chat.setError}
          />
          {overlay === "models" && <ModelPicker chat={chat} onClose={close} onSwitchAgent={(id) => void chat.switchAgent(id)} />}
        </main>
      )}

      <RightPanel tab={tab} setTab={setTab} workspace={workspace} chat={chat} onOpenChild={openChild} />

      {overlay && typeof overlay === "object" && <CommandPalette root={overlay.palette} onClose={close} />}
    </div>
  );
}

function EmptyState({ workspace, sessionId, loading, agent }: { workspace: string | null; sessionId: string | null; loading: boolean; agent: string }) {
  if (!workspace)
    return (
      <EmptySession title="Choose a workspace">
        <div>Sessions are kept per workspace. Pick one on the left, or open a folder with the folder button.</div>
      </EmptySession>
    );
  if (sessionId) return loading ? <div className="muted" style={{ margin: "auto" }}>Loading the session…</div> : null;
  return (
    <EmptySession title={`New session in ${baseName(workspace)}`}>
      <div className="mono" style={{ fontSize: 12 }}>{shortPath(workspace)}</div>
      <div>
        The first message starts a session as <b style={{ color: agent === "plan" ? "var(--bl)" : "var(--at)", fontWeight: 500 }}>{agent}</b>. By default the cognition guides you; turn on Delegate to have it do the work.
      </div>
    </EmptySession>
  );
}
