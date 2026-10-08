import { ArrowLeft, CheckCircle, CircleNotch, Compass, MagnifyingGlass, Palette, Plugs, ShieldCheck, Stack, TreeStructure, WarningCircle } from "@phosphor-icons/react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useAgents, useLayers, useModels, useSettings } from "../api/queries";
import { baseName, modKey } from "../lib/format";
import { fieldOf, fileLines, isSection, searchIndex, searchMatches, SECTIONS, statusLine, summaries, type SectionId } from "../lib/settings";
import type { Route } from "../router";
import type { ThemeId } from "../theme/themes";
import type { SectionProps } from "./context";
import { Gateways } from "./Gateways";
import { Layers } from "./Layers";
import { Appearance, Permissions, Subagents } from "./Simple";
import { useSettingsSave } from "./useSettingsSave";

const NAV: Record<SectionId, { label: string; icon: ReactNode }> = {
  gateways: { label: "Gateways", icon: <Plugs size={16} /> },
  layers: { label: "Layers", icon: <Stack size={16} /> },
  permissions: { label: "Permissions", icon: <ShieldCheck size={16} /> },
  subagents: { label: "Subagents", icon: <TreeStructure size={16} /> },
  appearance: { label: "Appearance", icon: <Palette size={16} /> },
};

interface Props {
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
  theme: ThemeId;
  setTheme: (id: ThemeId) => void;
}

export function SettingsPage({ route, navigate, theme, setTheme }: Props) {
  const workspace = route.workspace;
  const section: SectionId = isSection(route.section) ? route.section : "layers";
  const settings = useSettings(workspace);
  const models = useModels(workspace);
  const agents = useAgents(workspace, null);
  const layers = useLayers(workspace);
  const save = useSettingsSave(workspace);
  const [query, setQuery] = useState("");
  const search = useRef<HTMLInputElement>(null);
  const main = useRef<HTMLElement>(null);

  useEffect(() => {
    document.title = "Settings · WithNav";
  }, []);
  useEffect(() => {
    main.current?.scrollTo({ top: 0 });
  }, [section]);
  // ⌘F / Ctrl+F focuses the settings search; "/" too when no field has focus.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing = e.target instanceof HTMLElement && (e.target.closest("input, textarea, select") || e.target.isContentEditable);
      if (((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") || (e.key === "/" && !typing)) {
        e.preventDefault();
        search.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const body = settings.data;
  const agentList = useMemo(() => agents.data?.agents ?? [], [agents.data]);
  const index = useMemo(() => searchIndex(body, agentList), [body, agentList]);
  const matches = useMemo(() => searchMatches(index, query), [index, query]);
  const hits = (s: SectionId) => (matches ? index.filter((e) => e.section === s && matches.has(e.id)).length : null);
  const sum = summaries(body);
  const status = statusLine(save.state, save.state.lastKey ? fieldOf(body, save.state.lastKey)?.applies : undefined);
  const go = (s: SectionId) => navigate({ section: s });

  // While searching, a section without a match gives way to the first one that has some.
  useEffect(() => {
    if (!matches || !matches.size) return;
    if (hits(section)) return;
    const first = SECTIONS.find((s) => hits(s));
    if (first) navigate({ section: first }, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [matches]);

  const props: SectionProps | null =
    body && workspace
      ? {
          workspace,
          body,
          models: models.data?.models ?? [],
          modelsLoading: models.isPending,
          agents: agentList,
          layers: layers.data ?? [],
          save,
          visible: (id) => !matches || matches.has(id),
          go,
          theme,
          setTheme,
        }
      : null;

  const files = fileLines(body?.paths);

  return (
    <div className="st-shell">
      <header className="topbar st-top">
        <span className="brand">
          <Compass size={16} aria-hidden />
          WithNav
        </span>
        <span className="crumb">Settings</span>
        <div className="topbar-right">
          <label className="st-search">
            <MagnifyingGlass size={13} aria-hidden />
            <input ref={search} value={query} onChange={(e) => setQuery(e.target.value)} onKeyDown={(e) => e.key === "Escape" && setQuery("")} placeholder="Search settings" aria-label="Search settings" title={`Search settings (${modKey("F")})`} />
          </label>
          {body && (
            <span className={`st-status ${status.tone}`} role="status" title={save.state.message ?? undefined}>
              {status.tone === "busy" ? <CircleNotch size={13} className="spin" aria-hidden /> : status.tone === "ok" ? <CheckCircle size={13} aria-hidden /> : <WarningCircle size={13} aria-hidden />}
              <span className="text">{status.text}</span>
            </span>
          )}
        </div>
      </header>

      <nav className="st-side" aria-label="Settings sections">
        <button type="button" className="st-back" onClick={() => navigate({ page: "chat", section: null })}>
          <ArrowLeft size={13} aria-hidden /> Back to {workspace ? baseName(workspace) : "chat"}
        </button>
        {SECTIONS.map((s) => {
          const n = hits(s);
          return (
            <button key={s} type="button" className="st-nav" aria-current={s === section ? "page" : undefined} onClick={() => go(s)} disabled={n === 0}>
              <span className="icon">{NAV[s].icon}</span>
              <span className="text">
                <span className="l">
                  {NAV[s].label}
                  {n !== null && n > 0 && <span className="hits">{n}</span>}
                </span>
                <span className="s">{n === 0 ? "no match" : sum[s]}</span>
              </span>
            </button>
          );
        })}
        <div className="st-files mono" title={body ? [body.paths.config, body.paths.models].filter(Boolean).join("\n") : undefined}>
          {files.map((line) => (
            <div key={line}>{line}</div>
          ))}
        </div>
      </nav>

      <main className="st-main" ref={main}>
        {!workspace ? (
          <Notice title="Choose a workspace first">
            Settings read the catalogue and the agents of a workspace.{" "}
            <button type="button" className="link-btn" onClick={() => navigate({ page: "chat", section: null })}>
              Back to chat
            </button>
          </Notice>
        ) : settings.isError ? (
          <Notice title="Settings are not available">{settings.error instanceof Error ? settings.error.message : String(settings.error)}</Notice>
        ) : !props ? (
          <div className="muted st-loading">Loading the settings…</div>
        ) : (
          <div className="st-section" key={section}>
            {section === "gateways" && <Gateways {...props} />}
            {section === "layers" && <Layers {...props} />}
            {section === "permissions" && <Permissions {...props} />}
            {section === "subagents" && <Subagents {...props} />}
            {section === "appearance" && <Appearance {...props} />}
          </div>
        )}
      </main>
    </div>
  );
}

function Notice({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="st-notice" role="alert">
      <b>{title}</b>
      <div>{children}</div>
    </div>
  );
}
