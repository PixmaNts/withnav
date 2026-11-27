import { GitFork, Notebook } from "@phosphor-icons/react";
import { useNeedsYou } from "../api/queries";
import type { Route } from "../router";
import "./memory.css";

/** The register's entries at the top of the sidebar; the badge counts what waits for the Captain. */
export function MemoryNav({ route, navigate }: { route: Route; navigate: (next: Partial<Route>) => void }) {
  const waiting = useNeedsYou().data?.length ?? 0;
  const go = (page: "memory" | "projects") => navigate({ page, item: null, view: null });
  return (
    <div className="mm-nav" role="group" aria-label="Register">
      <button type="button" className="mm-nav-item" aria-current={route.page === "memory" ? "page" : undefined} onClick={() => go("memory")}>
        <Notebook size={14} aria-hidden /> Memory
        {waiting > 0 && (
          <span className="mm-badge" title="Waiting for you" aria-label={`${waiting} waiting for you`}>
            {waiting}
          </span>
        )}
      </button>
      <button type="button" className="mm-nav-item" aria-current={route.page === "projects" ? "page" : undefined} onClick={() => go("projects")}>
        <GitFork size={14} aria-hidden /> Projects
      </button>
    </div>
  );
}
