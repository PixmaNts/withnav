// Hash routes, so the server can serve one `index.html` from `/` with no
// fallback rule. Pages to come (InstinctFlow) add a `page` value.
//
//   #/                                  chat, no workspace chosen
//   #/chat?ws=<path>                    chat, new session in a workspace
//   #/chat?ws=<path>&s=<id>             chat, one session
//   #/chat?ws=<path>&s=<id>&sub=<id>    subagent view of a child of that session
//   #/settings?ws=<path>&section=<id>   settings (gateways, layers, permissions, subagents, appearance)

import { useCallback, useEffect, useState } from "react";

export interface Route {
  page: "chat" | "settings";
  workspace: string | null;
  session: string | null;
  subagent: string | null;
  /** The settings section; null on the chat page. */
  section: string | null;
}

export function parseRoute(hash: string): Route {
  const [path, query = ""] = hash.replace(/^#/, "").split("?");
  const params = new URLSearchParams(query);
  const settings = path === "/settings";
  return {
    page: settings ? "settings" : "chat",
    workspace: params.get("ws"),
    session: settings ? null : params.get("s"),
    subagent: settings ? null : params.get("sub"),
    section: settings ? params.get("section") : null,
  };
}

export function formatRoute(route: Route): string {
  const params = new URLSearchParams();
  if (route.workspace) params.set("ws", route.workspace);
  // The settings page keeps the session in memory (Back returns to it) but not in its address.
  if (route.page === "chat" && route.session) params.set("s", route.session);
  if (route.page === "chat" && route.subagent) params.set("sub", route.subagent);
  if (route.page === "settings" && route.section) params.set("section", route.section);
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
