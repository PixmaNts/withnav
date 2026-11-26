// Shared fixtures and mounting for the memory and projects page tests.
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { MemoryItem, MemoryItemDetail, MemoryVersion } from "../api/types";

export const version = (n: number, over: Partial<MemoryVersion> = {}): MemoryVersion => ({
  version: n,
  change: n === 1 ? "created" : "updated",
  title: "Judge permissions on consequence",
  body: `Body of version ${n}`,
  why: null,
  quote: null,
  scope: "global",
  scope_ref: "",
  source_actor: "captain",
  drafted_by: "captain",
  confirmed_by: "captain",
  source_session_id: "01HSESSION000000000000AAAA",
  source_event_seq: null,
  reason: null,
  at: "2026-11-20T10:00:00Z",
  ...over,
});

export const item = (id: string, over: Partial<MemoryItem> = {}, current: Partial<MemoryVersion> = {}): MemoryItem => {
  const v = version(over.current_version ?? 1, current);
  return {
    id,
    kind: "decision",
    scope: v.scope,
    scope_ref: v.scope_ref,
    status: "active",
    current_version: v.version,
    pinned: false,
    created_at: v.at,
    updated_at: v.at,
    current: v,
    ...over,
  };
};

export const detail = (base: MemoryItem, versions: MemoryVersion[]): MemoryItemDetail => ({ ...base, versions });

export interface Mounted {
  host: HTMLElement;
  root: Root;
  client: QueryClient;
  unmount: () => Promise<void>;
}

export async function mount(node: React.ReactNode): Promise<Mounted> {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity }, mutations: { retry: false } } });
  await act(async () => root.render(<QueryClientProvider client={client}>{node}</QueryClientProvider>));
  await settle();
  return {
    host,
    root,
    client,
    unmount: async () => {
      await act(async () => root.unmount());
      client.clear();
      host.remove();
    },
  };
}

/** Lets pending promises and the state they set run. */
export const settle = async () => {
  for (let i = 0; i < 4; i++) await act(async () => { await new Promise((r) => setTimeout(r, 0)); });
};

export const click = async (el: Element | null | undefined) => {
  if (!el) throw new Error("nothing to click");
  await act(async () => (el as HTMLElement).click());
  await settle();
};

export const byText = (host: ParentNode, selector: string, text: string | RegExp): HTMLElement | undefined =>
  [...host.querySelectorAll<HTMLElement>(selector)].find((el) => (typeof text === "string" ? el.textContent?.trim() === text : text.test(el.textContent ?? "")));

export const type = async (el: HTMLInputElement | HTMLTextAreaElement, value: string) => {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  await act(async () => {
    Object.getOwnPropertyDescriptor(proto, "value")!.set!.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
};

export const choose = async (el: HTMLSelectElement, value: string) => {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")!.set!.call(el, value);
    el.dispatchEvent(new Event("change", { bubbles: true }));
  });
};
