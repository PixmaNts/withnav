// Hash routes, so the server can serve one `index.html` from `/` with no
// fallback rule.
//
//   #/                                  chat, no workspace chosen
//   #/chat?ws=<path>                    chat, new session in a workspace
//   #/chat?ws=<path>&s=<id>             chat, one session
//   #/chat?ws=<path>&s=<id>&sub=<id>    subagent view of a child of that session
//   #/settings?ws=<path>&section=<id>   settings (gateways, layers, permissions, subagents, appearance)
//   #/studio?ws=<path>&flow=<name>&loc=<location>   InstinctFlow Studio; `&mock=1` uses the in-memory API
//   #/memory?ws=<path>&item=<id>&view=needs   the memory register; `item` opens one item, `view=needs` the waiting changes
//   #/projects?ws=<path>&item=<project id>    projects, groups and their sessions

import { useCallback, useEffect, useState } from "react";

export interface Route {
  page: "chat" | "settings" | "studio" | "memory" | "projects";
  workspace: string | null;
  session: string | null;
  subagent: string | null;
  /** The settings section; null on the chat page. */
  section: string | null;
  /** Studio: the open flow and its location (workspace, global, builtin). */
  flow?: string | null;
  loc?: string | null;
  /** Studio: use the in-memory flows API instead of the server's. */
  mock?: boolean;
  /** Memory: the open item id. Projects: the open project id. */
  item?: string | null;
  /** Memory: `needs` shows the changes that wait for the Captain instead of the list. */
  view?: string | null;
}

export function parseRoute(hash: string): Route {
  const [path, query = ""] = hash.replace(/^#/, "").split("?");
  const params = new URLSearchParams(query);
  const settings = path === "/settings";
  const studio = path === "/studio";
  const memory = path === "/memory";
  const projects = path === "/projects";
  const chat = !settings && !studio && !memory && !projects;
  return {
    page: settings ? "settings" : studio ? "studio" : memory ? "memory" : projects ? "projects" : "chat",
    workspace: params.get("ws"),
    session: chat ? params.get("s") : null,
    subagent: chat ? params.get("sub") : null,
    section: settings ? params.get("section") : null,
    flow: studio ? params.get("flow") : null,
    loc: studio ? params.get("loc") : null,
    mock: studio && params.get("mock") === "1",
    item: memory || projects ? params.get("item") : null,
    view: memory ? params.get("view") : null,
  };
}

export function formatRoute(route: Route): string {
  const params = new URLSearchParams();
  if (route.workspace) params.set("ws", route.workspace);
  // The settings page keeps the session in memory (Back returns to it) but not in its address.
  if (route.page === "chat" && route.session) params.set("s", route.session);
  if (route.page === "chat" && route.subagent) params.set("sub", route.subagent);
  if (route.page === "settings" && route.section) params.set("section", route.section);
  if (route.page === "studio" && route.flow) params.set("flow", route.flow);
  if (route.page === "studio" && route.flow && route.loc) params.set("loc", route.loc);
  if (route.page === "studio" && route.mock) params.set("mock", "1");
  if ((route.page === "memory" || route.page === "projects") && route.item) params.set("item", route.item);
  if (route.page === "memory" && route.view) params.set("view", route.view);
  const query = params.toString();
  return `#/${route.page}${query ? `?${query}` : ""}`;
}

export function useRoute(): [Route, (next: Partial<Route>, replace?: boolean) => void] {
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));
  useEffect(() => {
    const onHash = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  const navigate = useCallback((next: Partial<Route>, replace = false) => {
    setRoute((current) => {
      const merged = { ...current, ...next };
      const hash = formatRoute(merged);
      if (hash !== window.location.hash) {
        if (replace) history.replaceState(null, "", hash);
        else history.pushState(null, "", hash);
      }
      return merged;
    });
  }, []);
  return [route, navigate];
}
