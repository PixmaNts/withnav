// Side columns that turn into drawers on narrow screens. Shared by the chat page, the Studio and
// Settings: at wide widths a drawer is a plain grid column; below its breakpoint it slides over the
// page, opened from a top-bar button, closed by Escape, the backdrop or its close button.

import { X } from "@phosphor-icons/react";
import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent, type ReactNode } from "react";

/** Tablet: the chat's right panel and the Studio inspector leave the grid. */
export const TABLET = "(max-width: 1100px)";
/** Mobile: one column; the left lists leave the grid too. */
export const MOBILE = "(max-width: 700px)";

export function useMediaQuery(query: string): boolean {
  const subscribe = useCallback(
    (onChange: () => void) => {
      const list = window.matchMedia?.(query);
      list?.addEventListener("change", onChange);
      return () => list?.removeEventListener("change", onChange);
    },
    [query],
  );
  return useSyncExternalStore(subscribe, () => window.matchMedia?.(query).matches ?? false, () => false);
}

export interface DrawerState {
  /** The screen is narrow enough for this column to be a drawer. */
  active: boolean;
  open: boolean;
  setOpen: (open: boolean) => void;
}

/** A drawer that is closed whenever the screen crosses its breakpoint. */
export function useDrawer(query: string): DrawerState {
  const active = useMediaQuery(query);
  const [open, setOpen] = useState(false);
  useEffect(() => setOpen(false), [active]);
  return { active, open: active && open, setOpen };
}

const FOCUSABLE = 'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

interface Props {
  id: string;
  side: "left" | "right";
  state: DrawerState;
  /** Names the drawer for screen readers and in its header. */
  label: string;
  className?: string;
  children: ReactNode;
}

export function Drawer({ id, side, state, label, className = "", children }: Props) {
  const { active, open, setOpen } = state;
  const box = useRef<HTMLDivElement>(null);
  // The element that had focus when the drawer opened (its top-bar button); it gets focus back.
  const opener = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open) return;
    opener.current = document.activeElement as HTMLElement | null;
    // Focus lands on what is current in the drawer (the open session, the selected tab), else its first control.
    const first =
      box.current?.querySelector<HTMLElement>(`.drawer-body :is([aria-current="page"], [aria-selected="true"]):is(${FOCUSABLE})`) ??
      box.current?.querySelector<HTMLElement>(`.drawer-body :is(${FOCUSABLE})`) ??
      box.current?.querySelector<HTMLElement>(FOCUSABLE);
    first?.focus();
    return () => {
      const back = opener.current;
      opener.current = null;
      if (back?.isConnected && (!document.activeElement || document.activeElement === document.body || box.current?.contains(document.activeElement))) back.focus();
    };
  }, [open]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!open || e.defaultPrevented) return;
    if (e.key === "Escape") {
      e.preventDefault();
      setOpen(false);
    } else if (e.key === "Tab") {
      // The page behind is covered: Tab cycles inside the drawer.
      const all = [...(box.current?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? [])];
      if (!all.length) return;
      const at = all.indexOf(document.activeElement as HTMLElement);
      if (e.shiftKey && at <= 0) (e.preventDefault(), all[all.length - 1]!.focus());
      else if (!e.shiftKey && at === all.length - 1) (e.preventDefault(), all[0]!.focus());
    }
  };

  // One tree at every width, so the column's own state survives crossing the breakpoint. At wide
  // widths the wrappers are `display: contents` and the column sits in the page grid as before.
  return (
    <>
      {open && <div className="drawer-backdrop" aria-hidden onMouseDown={() => setOpen(false)} />}
      <div
        ref={box}
        id={id}
        className={`drawer ${className}`}
        data-drawer={active ? side : undefined}
        data-open={open || undefined}
        role={active ? "dialog" : undefined}
        aria-modal={open || undefined}
        aria-label={active ? label : undefined}
        inert={active && !open}
        onKeyDown={active ? onKeyDown : undefined}
      >
        {active && (
          <div className="drawer-head">
            <span>{label}</span>
            <button type="button" className="icon-btn" aria-label={`Close ${label.toLowerCase()}`} title="Close (Esc)" onClick={() => setOpen(false)}>
              <X size={16} aria-hidden />
            </button>
          </div>
        )}
        <div className="drawer-body">{children}</div>
      </div>
    </>
  );
}

/** The top-bar button that opens a drawer; shown only while the column is one. */
export function DrawerButton({ target, state, label, className = "", children }: { target: string; state: DrawerState; label: string; className?: string; children: ReactNode }) {
  if (!state.active) return null;
  return (
    <button type="button" className={`bar-btn ${className}`} aria-controls={target} aria-expanded={state.open} aria-label={label} title={label} onClick={() => state.setOpen(!state.open)}>
      {children}
    </button>
  );
}
