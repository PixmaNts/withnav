import { List } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { BrandMark } from "../components/Crew";
import { Sidebar } from "../components/Sidebar";
import { Drawer, DrawerButton, MOBILE, useDrawer } from "../lib/drawer";
import { baseName } from "../lib/format";
import { useWorkspaceExpansion } from "../lib/workspaceExpansion";
import type { Route } from "../router";
import "./memory.css";

interface Props {
  route: Route;
  navigate: (next: Partial<Route>, replace?: boolean) => void;
  /** The page's name in the top bar. */
  title: string;
  children: ReactNode;
}

/** The chat's frame (top bar and workspace sidebar) around a register page. */
export function PageShell({ route, navigate, title, children }: Props) {
  const side = useDrawer(MOBILE);
  const expansion = useWorkspaceExpansion(route.workspace);
  // A workspace or session picked in the sidebar leaves the register and opens in the chat.
  const toChat = (next: Partial<Route>) => {
    side.setOpen(false);
    navigate({ page: "chat", item: null, view: null, ...next });
  };
  return (
    <div className="shell mm-shell">
      <header className="topbar">
        <DrawerButton target="mm-side" state={side} label="Workspaces and sessions" className="side-toggle">
          <List size={18} aria-hidden />
        </DrawerButton>
        <span className="brand">
          <BrandMark />
          <span className="brand-name">WithNav</span>
        </span>
        <span className="crumb">
          {route.workspace ? baseName(route.workspace) : "WithNav"} / {title}
        </span>
      </header>
      <Drawer id="mm-side" side="left" state={side} label="Workspaces">
        <Sidebar route={route} navigate={toChat} expansion={expansion} />
      </Drawer>
      <main className="main mm-main">{children}</main>
    </div>
  );
}
