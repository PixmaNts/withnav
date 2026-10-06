// Hash routes, so the server can serve one `index.html` from `/` with no
// fallback rule. Pages to come (settings, InstinctFlow) add a `page` value.
//
//   #/                                  chat, no workspace chosen
//   #/chat?ws=<path>                    chat, new session in a workspace
//   #/chat?ws=<path>&s=<id>             chat, one session
//   #/chat?ws=<path>&s=<id>&sub=<id>    subagent view of a child of that session

import { useCallback, useEffect, useState } from "react";

export interface Route {
  page: "chat";
  workspace: string | null;
  session: string | null;
  subagent: string | null;
}

export function parseRoute(hash: string): Route {
  const [path, query = ""] = hash.replace(/^#/, "").split("?");
  const params = new URLSearchParams(query);
  void path;
  return {
    page: "chat",
    workspace: params.get("ws"),
    session: params.get("s"),
    subagent: params.get("sub"),
  };
}

export function formatRoute(route: Route): string {
  const params = new URLSearchParams();
  if (route.workspace) params.set("ws", route.workspace);
  if (route.session) params.set("s", route.session);
  if (route.subagent) params.set("sub", route.subagent);
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
