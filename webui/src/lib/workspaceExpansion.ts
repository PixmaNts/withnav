import { useCallback, useEffect, useState } from "react";
import { migratedValue } from "./storage";

export const EXPANDED_WORKSPACES_KEY = "withnav.expanded-workspaces";

export function readExpandedWorkspaces(current: string | null, storage: Pick<Storage, "getItem" | "setItem" | "removeItem">): Set<string> {
  try {
    const saved = migratedValue(storage, EXPANDED_WORKSPACES_KEY, "navai.expanded-workspaces");
    if (saved !== null) {
      const paths: unknown = JSON.parse(saved);
      if (Array.isArray(paths) && paths.every((path) => typeof path === "string" && path.startsWith("/"))) return new Set(paths);
    }
  } catch {
    // Browser storage can be unavailable; workspace controls still work in memory.
  }
  return new Set(current ? [current] : []);
}

export function setWorkspaceExpanded(paths: ReadonlySet<string>, path: string, expanded: boolean): Set<string> {
  const next = new Set(paths);
  if (expanded) next.add(path);
  else next.delete(path);
  return next;
}

export function useWorkspaceExpansion(current: string | null) {
  const [expanded, setExpanded] = useState(() => {
    try {
      return readExpandedWorkspaces(current, localStorage);
    } catch {
      return new Set(current ? [current] : []);
    }
  });
  useEffect(() => {
    try {
      localStorage.setItem(EXPANDED_WORKSPACES_KEY, JSON.stringify([...expanded]));
    } catch {
      // A blocked or full browser store must not prevent expanding a workspace.
    }
  }, [expanded]);
  const setOpen = useCallback((path: string, open: boolean) => setExpanded((paths) => setWorkspaceExpanded(paths, path, open)), []);
  const collapseAll = useCallback(() => setExpanded(new Set()), []);
  const expandAll = useCallback((paths: string[]) => setExpanded(new Set(paths)), []);
  return { expanded, setOpen, collapseAll, expandAll };
}
