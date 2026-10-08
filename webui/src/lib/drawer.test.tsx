// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { Drawer, DrawerButton, useDrawer } from "./drawer";

let narrow = true;
let container: HTMLDivElement;
let root: Root;

function Harness() {
  const state = useDrawer("(max-width: 700px)");
  return (
    <>
      <DrawerButton target="d" state={state} label="Workspaces and sessions">
        menu
      </DrawerButton>
      <Drawer id="d" side="left" state={state} label="Workspaces">
        <nav>
          <button type="button">first</button>
          <button type="button" aria-current="page">current</button>
        </nav>
      </Drawer>
    </>
  );
}

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  window.matchMedia = ((query: string) => ({ matches: narrow, media: query, addEventListener: () => {}, removeEventListener: () => {} })) as unknown as typeof window.matchMedia;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const $ = (sel: string) => container.querySelector<HTMLElement>(sel);

describe("drawer", () => {
  it("is a plain column with no button on a wide screen", () => {
    narrow = false;
    act(() => root.render(<Harness />));
    expect($("button[aria-controls]")).toBeNull();
    expect($('[role="dialog"]')).toBeNull();
    expect($("#d")!.hasAttribute("inert")).toBe(false);
    expect($("#d")!.textContent).toContain("current");
  });

  it("opens from its button, focuses the current item, closes on Escape and gives focus back", () => {
    narrow = true;
    act(() => root.render(<Harness />));
    const button = $("button[aria-controls]")!;
    const drawer = $("#d")!;
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(drawer.getAttribute("role")).toBe("dialog");
    expect(drawer.getAttribute("aria-label")).toBe("Workspaces");
    expect(drawer.hasAttribute("inert")).toBe(true);

    act(() => button.focus());
    act(() => button.click());
    expect(button.getAttribute("aria-expanded")).toBe("true");
    expect(drawer.getAttribute("aria-modal")).toBe("true");
    expect(drawer.hasAttribute("inert")).toBe(false);
    expect(document.activeElement?.textContent).toBe("current");

    act(() => document.activeElement!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(drawer.hasAttribute("inert")).toBe(true);
    expect(document.activeElement).toBe(button);
  });

  it("keeps Tab inside the open drawer", () => {
    narrow = true;
    act(() => root.render(<Harness />));
    act(() => $("button[aria-controls]")!.click());
    const close = $('.drawer-head button')!;
    const last = [...container.querySelectorAll<HTMLButtonElement>("#d nav button")].pop()!;
    act(() => last.focus());
    act(() => last.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true })));
    expect(document.activeElement).toBe(close);
  });
});
