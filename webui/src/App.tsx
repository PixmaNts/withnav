import { BrandMark, Crew, crewLabel } from "./components/Crew";
import { Compass, FlowArrow, GearSix, WifiSlash } from "@phosphor-icons/react";
import { useQueryClient } from "@tanstack/react-query";
import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api/client";
import { onUnauthorized } from "./api/http";
import { keys, useCommands, useSessions, useSettings, useWorkspaces } from "./api/queries";
import { useWorkspaceExpansion } from "./lib/workspaceExpansion";
import { CommandPalette, type PaletteCommand, type PalettePage } from "./components/CommandPalette";
import { Composer, type ComposerHandle } from "./components/Composer";
import { ModelPicker } from "./components/ModelPicker";
import { RightPanel, type PanelTab } from "./components/RightPanel";
import { Sidebar, sidebarWorkspaces } from "./components/Sidebar";
import { launchedReason, RemoveWorkspaceDialog, type RemoveRequest } from "./components/WorkspaceActions";
import { SubagentView } from "./components/SubagentView";
import { EmptySession, Transcript } from "./components/Transcript";
import { sessionTitle, type ClientCommand } from "./lib/compose";
import { baseName, modKey, shortPath } from "./lib/format";
import { useRoute, type Route } from "./router";
import { useChat } from "./session/useChat";
import { serverTheme } from "./lib/settings";
import { SettingsPage } from "./settings/SettingsPage";
import { applyTheme, storedTheme, THEMES, type ThemeId } from "./theme/themes";

// The Studio brings React Flow; it loads only when opened.
const StudioPage = lazy(() => import("./studio/StudioPage").then((m) => ({ default: m.StudioPage })));

type Overlay = null | "models" | { palette: PalettePage };

export function App() {
  const [expired, setExpired] = useState(false);
  useEffect(() => onUnauthorized(() => setExpired(true)), []);
  const [route, navigate] = useRoute();
  const [theme, setTheme] = useAppTheme(route.workspace);
  if (expired) return <Expired />;
  if (route.page === "settings") return <SettingsPage route={route} navigate={navigate} theme={theme} setTheme={setTheme} />;
  if (route.page === "studio")
    return (
      <Suspense fallback={<div className="fullpage muted">Loading the Studio…</div>}>
        <StudioPage route={route} navigate={navigate} />
      </Suspense>
    );
  return <ChatPage route={route} navigate={navigate} theme={theme} setTheme={setTheme} />;
}

/**
 * The theme: config.json's `tui.theme` (shared with the terminal client) wins; the choice saved in
 * this browser is the fallback while the server has none. The server value is adopted when it
 * changes, not on every refetch, so a theme picked from the palette holds until config.json moves.
 */
function useAppTheme(workspace: string | null): [ThemeId, (id: ThemeId) => void] {
  const [theme, setTheme] = useState<ThemeId>(storedTheme);
  const settings = useSettings(workspace);
  const fromServer = serverTheme(settings.data);
  const adopted = useRef<ThemeId | null>(null);
  useEffect(() => {
    if (fromServer && fromServer !== adopted.current) {
      adopted.current = fromServer;
      setTheme(fromServer);
    }
  }, [fromServer]);
  useEffect(() => applyTheme(theme), [theme]);
  return [theme, setTheme];
}

function Expired() {
  return (
    <div className="fullpage">
      <div className="box" role="alert">
        <h1>
          <Compass size={18} aria-hidden /> Session expired
        </h1>
        <div>
          The WithNav server no longer accepts this page. Relaunch <code>nav web</code> and open the address it prints.
        </div>
      </div>
    </div>
  );
}

interface PageProps {
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
  theme: ThemeId;
  setTheme: (id: ThemeId) => void;
}

function ChatPage({ route, navigate, theme, setTheme }: PageProps) {
  const qc = useQueryClient();
  const [overlay, setOverlay] = useState<Overlay>(null);
  const [tab, setTab] = useState<PanelTab>("intent");
  const [removing, setRemoving] = useState<RemoveRequest | null>(null);
  const composer = useRef<ComposerHandle>(null);
  const workspace = route.workspace;
  const expansion = useWorkspaceExpansion(workspace);
  const workspaces = useWorkspaces();
  const expandAllWorkspaces = useCallback(() => expansion.expandAll([...new Set([...(workspaces.data ?? []).map((w) => w.path), ...(workspace ? [workspace] : [])])]), [expansion.expandAll, workspaces.data, workspace]);
  const onCreated = useCallback((id: string) => navigate({ session: id, subagent: null }, true), [navigate]);
  const chat = useChat(workspace, route.session, onCreated);
  const sessions = useSessions(workspace);
  const commands = useCommands();
  const sessionRow = sessions.data?.find((s) => s.id === route.session);
  const sidebarList = useMemo(() => sidebarWorkspaces(workspaces.data ?? [], workspace), [workspaces.data, workspace]);
  // The open workspace as the server lists it; one opened by path but not listed yet has nothing to remove.
  const currentWs = workspaces.data?.find((w) => w.path === workspace);
  const title = route.session ? sessionTitle(sessionRow?.title) : "New session";
  const childRow = route.subagent ? sessions.data?.find((s) => s.id === route.subagent) : undefined;
  const childAgent = (route.subagent && chat.view.children[route.subagent]?.agent) || childRow?.agent;
  const bypass = route.subagent ? childRow?.bypass === true : chat.bypass;
  const bypassReason = route.subagent ? "Subagent bypass is controlled by its parent" : chat.bypassReason;

  useEffect(() => {
    document.title = route.session ? `${title} · WithNav` : "WithNav";
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
  const openSettings = useCallback(() => navigate({ page: "settings", section: null }), [navigate]);
  const openStudio = useCallback(() => navigate({ page: "studio", flow: null, loc: null }), [navigate]);

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
      { id: "bypass", group: "Session", label: `Turn bypass ${bypass ? "off" : "on"}`, description: "Bypass ordinary permission prompts; protected requests still ask", hint: "", disabled: bypassReason, run: () => void chat.setBypass(!bypass) },
      { id: "agent", group: "Agent", label: `Switch to ${crewLabel(otherAgent)}`, hint: "tab", run: toggleAgent, disabled: workspace ? undefined : "Choose a workspace first" },
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
          items: children.map((c) => ({ id: c.id, group: c.status, label: crewLabel(c.agent), description: c.goal.slice(0, 80), run: () => openChild(c.id) })),
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
          items: (sessions.data ?? []).filter((s) => !s.parent_session_id).map((s) => ({ id: s.id, group: s.agent, label: sessionTitle(s.title), description: `${s.message_count} msgs`, run: () => navigate({ session: s.id, subagent: null }) })),
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
      ...(currentWs
        ? [
            { id: "ws-hide", group: "Workspace", label: `Remove ${currentWs.name} from list`, hint: "", disabled: launchedReason(currentWs), run: () => setRemoving({ ws: currentWs, kind: "hide", count: currentWs.session_count }) },
            {
              id: "ws-delete",
              group: "Workspace",
              label: `Delete sessions of ${currentWs.name}…`,
              hint: "",
              disabled: launchedReason(currentWs) ?? (currentWs.session_count ? undefined : "This folder has no session to delete."),
              run: () => setRemoving({ ws: currentWs, kind: "delete", count: currentWs.session_count }),
            },
          ]
        : []),
      { id: "theme", group: "View", label: "Theme…", hint: "", page: themePage },
      { id: "collapse-workspaces", group: "View", label: "Collapse all workspaces", hint: "", run: expansion.collapseAll },
      { id: "expand-workspaces", group: "View", label: "Expand all workspaces", hint: "", run: expandAllWorkspaces },
      { id: "settings", group: "View", label: "Open settings", hint: "", run: openSettings, disabled: workspace ? undefined : "Choose a workspace first" },
      { id: "studio", group: "View", label: "Open the InstinctFlow Studio", description: "Draw, test and reload flows", hint: "", run: openStudio, disabled: workspace ? undefined : "Choose a workspace first" },
    ];
    // The server's commands, minus the ones above already cover.
    const covered = new Set(["model", "new", "agent", "stop", "bypass"]);
    for (const c of commands.data ?? []) {
      if (covered.has(c.name)) continue;
      items.push({
        id: `server:${c.name}`,
        group: "Session",
        label: c.name === "intent" ? "Set the Course" : ["approve", "approve_plan", "confirm_route"].includes(c.name) ? "Confirm the Route" : c.description,
        hint: `/${c.name}`,
        disabled: hasSession ? undefined : "Send a first message to start the session",
        run: () => (c.args_hint.startsWith("<") ? composer.current?.fill(`/${c.name} `) : void runServer(c.name, "")),
      });
    }
    const order = ["Agent", "Session", "Workspace", "Extensions", "View"];
    items.sort((a, b) => order.indexOf(a.group) - order.indexOf(b.group));
    return { title: "Commands", placeholder: "Type a command or search…", items };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bypass, bypassReason, chat.setBypass, route.session, chat.view.children, chat.running, otherAgent, workspace, sessions.data, commands.data, themePage, skillsPage, openSettings, openStudio, expansion.collapseAll, expandAllWorkspaces, currentWs]);

  const runServer = async (name: string, args: string) => {
    if (name === "bypass" && ["on", "off"].includes(args.trim())) {
      if (bypassReason) { chat.setError(bypassReason); return; }
      await chat.setBypass(args.trim() === "on");
      return;
    }
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

  const crumbWs = workspace ? baseName(workspace) : "WithNav";
  const crumb = route.subagent ? `${title} / ${childAgent ?? "subagent"}` : workspace ? title : "Choose a workspace";

  return (
    <div className="shell">
      <header className="topbar">
        <span className="brand">
          <BrandMark />
          WithNav
        </span>
        <span className="crumb" title={workspace ?? undefined}>
          {crumbWs} / {crumb}
        </span>
        {connection && (
          <span className="conn" role="status">
            <WifiSlash size={13} aria-hidden /> {connection}
          </span>
        )}
        {bypass && <span className="bypass-badge" title="Bypass ordinary permission prompts; protected requests still ask">⚠ bypass</span>}
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
          <button type="button" className="bypass-toggle" aria-pressed={bypass} disabled={!!bypassReason} title={bypassReason ?? "Bypass ordinary permission prompts; protected requests still ask"} onClick={() => void chat.setBypass(!bypass)}>
            ⚠ Bypass {bypass ? "ON" : "OFF"}
          </button>
          <button type="button" className="icon-btn st-gear" onClick={openStudio} disabled={!workspace} aria-label="InstinctFlow Studio" title={workspace ? "InstinctFlow Studio: flows" : "Choose a workspace first"}>
            <FlowArrow size={16} />
          </button>
          <button type="button" className="icon-btn st-gear" onClick={openSettings} disabled={!workspace} aria-label="Settings" title={workspace ? "Settings" : "Choose a workspace first"}>
            <GearSix size={16} />
          </button>
          <button type="button" className="kbd" onClick={() => setOverlay({ palette: paletteWithSkills })} aria-label="Open the command palette" title="Command palette">
            {modKey("K")}
          </button>
        </div>
        {bypass && <span className="bypass-warning" role="status">Bypass ordinary permission prompts; protected requests still ask</span>}
      </header>

      <Sidebar route={route} navigate={navigate} expansion={expansion} onRemove={setRemoving} />

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
      {removing && <RemoveWorkspaceDialog request={removing} workspaces={sidebarList} route={route} navigate={navigate} onClose={() => setRemoving(null)} />}
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
        The first message starts a session as <b style={{ color: agent === "plan" ? "var(--bl)" : "var(--at)", fontWeight: 500 }}><Crew id={agent} /></b>. By default Nav guides you; turn on Delegate to have it do the work.
      </div>
    </EmptySession>
  );
}
