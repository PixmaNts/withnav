// @vitest-environment jsdom
import { act, StrictMode, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { Composer } from "./Composer";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
const MAX_IMAGE_BYTES = 3_932_160;
let root: Root | null = null;
afterEach(() => {
  if (root) act(() => root!.unmount());
  root = null;
  document.body.innerHTML = "";
});

function mount(overrides: Partial<ComponentProps<typeof Composer>> = {}) {
  const props: ComponentProps<typeof Composer> = {
    agent: "build", modelLabel: "model", pickerOpen: false, running: false, context: null, serverCommands: [], error: null,
    onOpenModels: vi.fn(), onToggleAgent: vi.fn(), onSend: vi.fn(async () => true), onServerCommand: vi.fn(async () => undefined),
    onClientCommand: vi.fn(), onStop: vi.fn(), onError: vi.fn(), ...overrides,
  };
  const host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  act(() => root!.render(<StrictMode><Composer {...props} /></StrictMode>));
  const area = host.querySelector("textarea")!;
  const send = () => [...host.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent?.includes("Send"))!;
  const chips = () => [...host.querySelectorAll(".pending-images li")].map((li) => li.textContent);
  return { host, props, area, send, chips };
}

function type(area: HTMLTextAreaElement, text: string) {
  Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(area, text);
  area.dispatchEvent(new Event("input", { bubbles: true }));
}

function image(name: string, size: number): File {
  const file = new File(["x"], name, { type: "image/png" });
  Object.defineProperty(file, "size", { value: size });
  return file;
}

function pick(host: HTMLElement, files: File[]) {
  const input = host.querySelector<HTMLInputElement>('input[type="file"]')!;
  Object.defineProperty(input, "files", { value: files, configurable: true });
  input.dispatchEvent(new Event("change", { bubbles: true }));
}

it("unlocks and keeps the draft when a server command rejects", async () => {
  const { area, send, props } = mount({ onServerCommand: vi.fn(async () => { throw new Error("server unreachable"); }) });
  act(() => type(area, "/compact"));
  await act(async () => send().click());
  expect(props.onServerCommand).toHaveBeenCalledWith("compact", "");
  expect(props.onError).toHaveBeenLastCalledWith("server unreachable");
  expect(send().disabled).toBe(false);
  expect(area.value).toBe("/compact");
});

it("unlocks and keeps the draft and images when sending rejects", async () => {
  const { host, area, send, chips, props } = mount({ onSend: vi.fn(async () => { throw new Error("offline"); }) });
  act(() => type(area, "hello"));
  act(() => pick(host, [image("a.png", 10)]));
  await act(async () => send().click());
  expect(props.onError).toHaveBeenLastCalledWith("offline");
  expect(send().disabled).toBe(false);
  expect(area.value).toBe("hello");
  expect(chips()).toHaveLength(1);
});

it("refuses an image above the server's byte cap before it is listed or uploaded", async () => {
  const { host, send, chips, props } = mount();
  act(() => pick(host, [image("big.png", MAX_IMAGE_BYTES + 1), image("edge.png", MAX_IMAGE_BYTES)]));
  expect(props.onError).toHaveBeenCalledTimes(1);
  expect(props.onError).toHaveBeenCalledWith("big.png: larger than 3.75 MiB, not attached.");
  expect(chips()).toHaveLength(1);
  expect(chips()[0]).toContain("edge.png");
  await act(async () => send().click());
  expect((props.onSend as ReturnType<typeof vi.fn>).mock.calls[0]![1].map((f: File) => f.name)).toEqual(["edge.png"]);
});

it("applies the byte cap to pasted images and keeps the count cap", () => {
  const { host, area, chips, props } = mount();
  act(() => pick(host, Array.from({ length: 7 }, (_, i) => image(`p${i}.png`, 10))));
  const paste = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(paste, "clipboardData", { value: { files: [image("huge.png", MAX_IMAGE_BYTES + 1), image("q1.png", 10), image("q2.png", 10)] } });
  act(() => { area.dispatchEvent(paste); });
  expect(props.onError).toHaveBeenLastCalledWith("huge.png: larger than 3.75 MiB, not attached. A message carries at most 8 images.");
  expect(chips()).toHaveLength(8);
  expect(chips().some((c) => c?.includes("huge.png"))).toBe(false);
});
