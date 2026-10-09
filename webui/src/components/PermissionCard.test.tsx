// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { PermissionCard } from "./PermissionCard";
import type { Permission } from "../session/reducer";

it("offers and sends the session config answer only for protected config", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  const answer = vi.fn(async () => {});
  const perm: Permission = { requestId: "r", toolCallId: "c", tool: "write", agent: "build", reason: "config", scope: {kind:"folder",value:"/config"}, arguments: "{}" };
  await act(async () => root.render(<PermissionCard perm={perm} onAnswer={answer} />));
  expect(host.textContent).not.toContain("Allow WithNav config for this session");
  await act(async () => root.render(<PermissionCard perm={{...perm, allowConfigSession:true}} onAnswer={answer} />));
  const button = [...host.querySelectorAll("button")].find(b => b.textContent === "Allow WithNav config for this session")!;
  expect(button).toBeDefined();
  await act(async () => button.click());
  expect(answer).toHaveBeenCalledWith("allow_config_session");
  await act(async () => root.unmount());
});
