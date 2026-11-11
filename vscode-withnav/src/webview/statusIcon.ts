/// <reference lib="dom" />

/** What a conversation is doing, as one icon: tabs and the session list share it. */
export type SessionStatus = "idle" | "running" | "waiting" | "done" | "error" | "active";

export const STATUS_LABEL: Record<SessionStatus, string> = {
  idle: "Empty",
  running: "Running",
  waiting: "Waiting for you",
  done: "Done",
  error: "Error",
  active: "Active",
};

const SHAPES: Record<SessionStatus, string> = {
  idle: '<circle class="st-ring" cx="8" cy="8" r="3.5"/>',
  running: '<circle class="st-track" cx="8" cy="8" r="5.5"/><path class="st-arc" d="M8 2.5a5.5 5.5 0 0 1 5.5 5.5"/>',
  waiting: '<circle class="st-fill" cx="8" cy="8" r="4.5"/>',
  done: '<path class="st-mark" d="m3.8 8.4 2.7 2.7 5.7-5.9"/>',
  error: '<path class="st-mark" d="m4.5 4.5 7 7m0-7-7 7"/>',
  active: '<circle class="st-fill" cx="8" cy="8" r="2.5"/>',
};

/** A small icon whose colour comes from the theme through CSS (`data-status`); the words stay in its label. */
export function statusIcon(status: SessionStatus, label = STATUS_LABEL[status]): HTMLElement {
  const icon = document.createElement("span");
  icon.className = "st-icon";
  icon.dataset.status = status;
  icon.setAttribute("role", "img");
  icon.setAttribute("aria-label", label);
  icon.title = label;
  icon.innerHTML = `<svg viewBox="0 0 16 16" aria-hidden="true">${SHAPES[status]}</svg>`;
  return icon;
}
