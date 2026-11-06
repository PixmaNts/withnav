/// <reference lib="dom" />
// The secret card: the Captain types a value the agent asked for with `request_secret`. The
// value leaves this card only in one `submitSecret` message to the host and is then cleared
// from the input; it is never put in webview state, a log or another message.
import type { SecretInfo, SecretNote, SecretRequest, SecretScope, SecretTtl, WebviewToHost } from "../chatProtocol";

export type SecretCardState = "pending" | "saving" | "saved" | "declined" | "revoked" | "expired";

export interface SecretCardEnv {
  post(message: WebviewToHost): void;
  sessionId(): string;
  readOnly(): boolean;
  /** The card changed what the conversation waits for. */
  changed(): void;
}

export interface SecretCard {
  readonly root: HTMLElement;
  readonly name: string;
  state(): SecretCardState;
  /** The server settled the request (`message.received.secret`). */
  settle(note: SecretNote): void;
  /** The outcome of the Captain's own action. */
  update(state: "saved" | "declined" | "revoked" | "error", secret?: SecretInfo, message?: string): void;
  /** The conversation's secrets as the server lists them now. */
  reconcile(items: SecretInfo[]): void;
  setReadOnly(readOnly: boolean): void;
  dispose(): void;
}

const svg = (d: string): string => `<svg class="icon" viewBox="0 0 16 16" aria-hidden="true"><path d="${d}"/></svg>`;
export const KEY_ICON = svg("M10.5 2a3.5 3.5 0 1 1-1.2 6.8L8 10.1V11H7v1H6v1.5H3V11l4.2-4.2A3.5 3.5 0 0 1 10.5 2Zm.7 2.3a.9.9 0 1 0 0 1.8.9.9 0 0 0 0-1.8Z");
const EYE = svg("M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8Zm6.5 2a2 2 0 1 0 0-4 2 2 0 0 0 0 4Z");
const EYE_OFF = svg("M2 2l12 12M6.6 6.6a2 2 0 0 0 2.8 2.8M4.2 4.6C2.5 5.8 1.5 8 1.5 8S4 12.5 8 12.5c1.2 0 2.3-.4 3.2-.9M7 3.6c.3 0 .7-.1 1-.1 4 0 6.5 4.5 6.5 4.5s-.5 1-1.5 2");
const PASTE = svg("M5.5 2.5h5v2h-5zM4 3.5H3v10h10v-10h-1M5.5 8h5M5.5 10.5h3");

const TTL_LABELS: [SecretTtl, string][] = [["1h", "1 h"], ["24h", "24 h"], ["session", "Session"]];
const SCOPE_LABELS: [SecretScope, string][] = [["session", "This session"], ["workspace", "This workspace"]];

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(className: string, label: string, action: () => void): HTMLButtonElement {
  const node = el("button", className, label);
  node.type = "button";
  node.addEventListener("click", action);
  return node;
}

/** "in 59 min", "in 23 h", or undefined once past. */
export function expiresIn(expiresAt: string | undefined, now = Date.now()): string | undefined {
  if (!expiresAt) return "when this conversation stops";
  const left = Date.parse(expiresAt) - now;
  if (!Number.isFinite(left) || left <= 0) return undefined;
  const minutes = Math.ceil(left / 60_000);
  return minutes < 60 ? `in ${minutes} min` : `in ${Math.round(minutes / 60)} h`;
}

export const scopeText = (scope: SecretScope | undefined): string => scope === "workspace" ? "this workspace" : "this session";

/** A segmented control: one pressed button among `options`. */
function segmented<T extends string>(label: string, options: [T, string][], initial: T, onChange: () => void): { root: HTMLElement; value(): T; set(value: T): void; disable(disabled: boolean): void } {
  const root = el("div", "secret-seg");
  root.setAttribute("role", "radiogroup");
  root.setAttribute("aria-label", label);
  let current = initial;
  const buttons = options.map(([value, text]) => {
    const node = button("secret-seg-btn", text, () => { current = value; paint(); onChange(); });
    node.setAttribute("role", "radio");
    node.dataset.value = value;
    root.append(node);
    return node;
  });
  const paint = (): void => {
    for (const node of buttons) {
      const on = node.dataset.value === current;
      node.setAttribute("aria-checked", String(on));
      node.classList.toggle("selected", on);
      node.tabIndex = on ? 0 : -1;
    }
  };
  root.addEventListener("keydown", event => {
    if (!["ArrowLeft", "ArrowRight"].includes(event.key)) return;
    event.preventDefault();
    const index = options.findIndex(([value]) => value === current);
    current = options[(index + (event.key === "ArrowRight" ? 1 : options.length - 1)) % options.length][0];
    paint(); onChange();
    buttons.find(node => node.dataset.value === current)?.focus();
  });
  paint();
  return { root, value: () => current, set(value) { current = value; paint(); }, disable(disabled) { for (const node of buttons) node.disabled = disabled; } };
}

export function createSecretCard(requestId: string, request: SecretRequest, agent: string, env: SecretCardEnv): SecretCard {
  let state: SecretCardState = "pending";
  let saved: { scope?: SecretScope; expiresAt?: string; last4?: string } = {};
  let expiryTimer: ReturnType<typeof setTimeout> | undefined;
  /** The form is open again to give a value after the request was settled. */
  let reopened = false;

  const root = el("article", "control-card secret-card");
  root.setAttribute("aria-label", `Secret requested: ${request.label}`);
  const head = el("div", "secret-head");
  const icon = el("span", "secret-icon");
  icon.innerHTML = KEY_ICON;
  const status = el("span", "secret-status");
  status.setAttribute("role", "status");
  head.append(icon, el("strong", "secret-label", request.label), status);
  const reason = el("p", "secret-reason", `For ${request.reason}. Requested by ${agent || "Nav"}.`);

  const form = el("div", "secret-form");
  const field = el("div", "secret-field");
  const input = el("input", "secret-input");
  input.type = "password";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.setAttribute("autocapitalize", "off");
  input.setAttribute("aria-label", `Value of ${request.name}`);
  input.placeholder = `Paste or type ${request.name}`;
  const eye = button("icon-button secret-eye", "", () => {
    const shown = input.type === "text";
    input.type = shown ? "password" : "text";
    eye.innerHTML = shown ? EYE : EYE_OFF;
    eye.setAttribute("aria-pressed", String(!shown));
    eye.setAttribute("aria-label", shown ? "Show the value" : "Hide the value");
    input.focus();
  });
  eye.innerHTML = EYE;
  eye.setAttribute("aria-pressed", "false");
  eye.setAttribute("aria-label", "Show the value");
  eye.title = "Show or hide (the value stays on this screen)";
  const paste = button("icon-button secret-paste", "", () => {
    void (async () => {
      try {
        const text = await navigator.clipboard.readText();
        if (text) { input.value = text.replace(/[\r\n]+$/, ""); refresh(); }
      } catch {
        outcome.textContent = "Paste with Ctrl+V (Cmd+V) in the field.";
      }
      input.focus();
    })();
  });
  paste.innerHTML = PASTE;
  paste.setAttribute("aria-label", "Paste from the clipboard");
  paste.title = "Paste from the clipboard";
  field.append(input, eye, paste);
  const options = el("div", "secret-options");
  const ttl = segmented("Availability", TTL_LABELS, request.defaultTtl, () => undefined);
  const scope = segmented("Scope", SCOPE_LABELS, request.defaultScope, () => undefined);
  const ttlRow = el("div", "secret-option");
  ttlRow.append(el("span", "secret-option-label", "Available"), ttl.root);
  const scopeRow = el("div", "secret-option");
  scopeRow.append(el("span", "secret-option-label", "For"), scope.root);
  options.append(ttlRow, scopeRow);
  const note = el("p", "secret-note");
  note.append("Kept out of the chat. The agent receives ", el("code", "", `$${request.name}`), ", not the value.");
  const actions = el("div", "control-actions secret-actions");
  const decline = button("control-secondary secret-decline", "Decline", () => {
    if (reopened) { reopened = false; render(); return; }
    if (!env.sessionId()) return;
    setBusy("Declining…");
    env.post({ type: "declineSecret", sessionId: env.sessionId(), requestId });
  });
  const save = button("control-primary secret-save", "Save", () => submit());
  actions.append(decline, save);
  form.append(field, options, note, actions);

  const result = el("div", "secret-result");
  const outcome = el("div", "control-outcome");
  outcome.setAttribute("role", "status");
  root.append(head, reason, form, result, outcome);

  input.addEventListener("input", () => refresh());
  input.addEventListener("keydown", event => {
    if (event.key === "Enter") { event.preventDefault(); submit(); }
    if (event.key === "Escape" && !reopened && state === "pending") { event.preventDefault(); decline.click(); }
  });

  function submit(): void {
    const value = input.value;
    if (!value || !env.sessionId() || env.readOnly()) return;
    const requested = state === "pending";
    // The value goes to the host once; the field is emptied at once so nothing keeps it.
    env.post({ type: "submitSecret", sessionId: env.sessionId(), requestId, name: request.name, value, ttl: ttl.value(), scope: scope.value() });
    input.value = "";
    input.type = "password";
    eye.innerHTML = EYE;
    eye.setAttribute("aria-pressed", "false");
    saved = { scope: scope.value() };
    reopened = false;
    if (requested) state = "saving";
    setBusy("Saving…");
  }

  function setBusy(text: string): void {
    for (const control of form.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input")) control.disabled = true;
    outcome.textContent = text;
  }

  function refresh(): void {
    save.disabled = env.readOnly() || !input.value;
  }

  function scheduleExpiry(): void {
    if (expiryTimer) clearTimeout(expiryTimer);
    if (!saved.expiresAt) return;
    const left = Date.parse(saved.expiresAt) - Date.now();
    if (!Number.isFinite(left)) return;
    // Long waits are re-armed: timers past ~24 days overflow.
    expiryTimer = setTimeout(() => { expiryTimer = undefined; if (state === "saved") { if (expiresIn(saved.expiresAt) === undefined) { state = "expired"; render(); } else scheduleExpiry(); } }, Math.min(Math.max(left, 0) + 500, 3_600_000));
  }

  function render(): void {
    root.dataset.state = state;
    const editing = state === "pending" || state === "saving" || reopened;
    form.hidden = !editing;
    for (const control of form.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button,input")) control.disabled = env.readOnly() || state === "saving";
    decline.textContent = reopened ? "Cancel" : "Decline";
    refresh();
    result.replaceChildren();
    if (state !== "saving") { outcome.textContent = ""; delete outcome.dataset.error; }
    switch (state) {
      case "pending":
        status.textContent = env.readOnly() ? "waiting for an answer in the parent conversation" : "waiting for your answer";
        break;
      case "saving":
        status.textContent = "saving…";
        break;
      case "saved": {
        const when = expiresIn(saved.expiresAt);
        status.textContent = "saved";
        const line = el("div", "secret-saved");
        line.append(el("code", "secret-masked", `$${request.name} ••••${saved.last4 ?? ""}`));
        line.append(el("span", "secret-expiry", `expires ${when ?? "now"}, ${scopeText(saved.scope)}`));
        const buttons = el("div", "control-actions");
        buttons.append(
          button("control-secondary", "Replace", () => { reopened = true; render(); input.focus(); }),
          button("control-secondary secret-revoke", "Revoke", () => {
            if (!env.sessionId()) return;
            outcome.textContent = "Revoking…";
            env.post({ type: "revokeSecret", sessionId: env.sessionId(), name: request.name, requestId });
          }),
        );
        for (const control of buttons.querySelectorAll("button")) control.disabled = env.readOnly();
        result.append(line, buttons);
        break;
      }
      case "declined": {
        status.textContent = "declined";
        result.append(el("p", "secret-declined", "Nav was told and continues without it."));
        if (!reopened) {
          const provide = button("control-secondary", "Provide anyway", () => { reopened = true; render(); input.focus(); });
          provide.disabled = env.readOnly();
          result.append(provide);
        }
        break;
      }
      case "revoked":
      case "expired":
        status.textContent = state;
        result.append(el("p", "secret-declined", state === "revoked" ? `$${request.name} was revoked: commands no longer receive it.` : `$${request.name} expired: commands no longer receive it.`));
        if (!reopened) {
          const provide = button("control-secondary", "Provide again", () => { reopened = true; render(); input.focus(); });
          provide.disabled = env.readOnly();
          result.append(provide);
        }
        break;
    }
    env.changed();
  }

  render();
  return {
    root,
    name: request.name,
    state: () => state,
    settle(noteValue) {
      if (noteValue.outcome === "declined") { state = "declined"; render(); return; }
      saved = { ...saved, scope: noteValue.scope ?? saved.scope, expiresAt: noteValue.expiresAt };
      state = expiresIn(noteValue.expiresAt) === undefined ? "expired" : "saved";
      scheduleExpiry();
      render();
    },
    update(next, secret, message) {
      if (next === "error") {
        // Back to the form, empty: the value is not kept for a retry.
        if (state === "saving") state = "pending";
        reopened = state !== "pending";
        render();
        outcome.textContent = message ?? "Could not save the secret.";
        outcome.dataset.error = "true";
        return;
      }
      if (next === "saved") {
        saved = { scope: secret?.scope ?? saved.scope, expiresAt: secret?.expiresAt, last4: secret?.last4 };
        // A pending request settles with the server's note; a later value is saved at once.
        if (state !== "pending" && state !== "saving") state = "saved";
        if (state === "saving") state = "saved";
        scheduleExpiry();
      } else if (next === "revoked") state = "revoked";
      else if (next === "declined") state = "declined";
      render();
    },
    reconcile(items) {
      const row = items.find(item => item.name === request.name);
      if (state === "saved" && !row) { state = expiresIn(saved.expiresAt) === undefined ? "expired" : "revoked"; render(); return; }
      if (row && (state === "saved" || state === "revoked" || state === "expired")) {
        saved = { scope: row.scope, expiresAt: row.expiresAt, last4: row.last4 };
        state = "saved";
        scheduleExpiry();
        render();
      }
    },
    setReadOnly() { render(); },
    dispose() { if (expiryTimer) clearTimeout(expiryTimer); input.value = ""; },
  };
}
