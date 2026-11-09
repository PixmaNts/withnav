import { ClipboardText, Eye, EyeSlash, Key } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import type { SecretView } from "../api/types";
import type { Question, SecretScope, SecretTtl } from "../session/reducer";

/** What the card can ask of the server. The value is passed once to `submit` and kept nowhere. */
export interface SecretActions {
  submit(requestId: string, body: { name?: string; value: string; ttl: SecretTtl; scope: SecretScope }): Promise<SecretView | undefined>;
  decline(requestId: string): Promise<void>;
  revoke(name: string): Promise<void>;
  /** The conversation's secrets as last listed, to show the last characters and a revocation. */
  list: SecretView[] | null;
}

const TTLS: [SecretTtl, string][] = [["1h", "1 h"], ["24h", "24 h"], ["session", "Session"]];
const SCOPES: [SecretScope, string][] = [["session", "This session"], ["workspace", "This workspace"]];

/** "in 59 min", "in 23 h", "when this conversation stops", or undefined once past. */
export function expiresIn(expiresAt: string | undefined, now = Date.now()): string | undefined {
  if (!expiresAt) return "when this conversation stops";
  const left = Date.parse(expiresAt) - now;
  if (!Number.isFinite(left) || left <= 0) return undefined;
  const minutes = Math.ceil(left / 60_000);
  return minutes < 60 ? `in ${minutes} min` : `in ${Math.round(minutes / 60)} h`;
}
export const scopeText = (scope?: SecretScope) => (scope === "workspace" ? "this workspace" : "this session");

function Segmented<T extends string>({ label, options, value, onChange, disabled }: { label: string; options: [T, string][]; value: T; onChange: (v: T) => void; disabled?: boolean }) {
  return (
    <div className="secret-seg" role="radiogroup" aria-label={label}>
      {options.map(([v, text]) => (
        <button key={v} type="button" role="radio" aria-checked={value === v} className={value === v ? "selected" : ""} disabled={disabled} onClick={() => onChange(v)}>
          {text}
        </button>
      ))}
    </div>
  );
}

export function SecretCard({ question, actions, readOnly }: { question: Question; actions?: SecretActions; readOnly?: boolean }) {
  const request = question.secret!;
  const requestId = question.questionCallId!;
  // The value lives in this component's memory only, until it is sent.
  const [value, setValue] = useState("");
  const [shown, setShown] = useState(false);
  const [ttl, setTtl] = useState<SecretTtl>(request.defaultTtl);
  const [scope, setScope] = useState<SecretScope>(request.defaultScope);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reopened, setReopened] = useState(false);
  const [local, setLocal] = useState<{ state: "saved" | "revoked"; view?: SecretView } | null>(null);
  const [, tick] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  // Saving, declining, revoking and pasting finish after an await: the card may be gone by then.
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    const timer = setInterval(() => tick((n) => n + 1), 30_000);
    return () => clearInterval(timer);
  }, []);

  const note = question.secretNote;
  const listed = actions?.list?.find((s) => s.name === request.name);
  const settled = note?.outcome === "stored" || local?.state === "saved";
  const expiresAt = listed?.expires_at ?? local?.view?.expires_at ?? note?.expiresAt;
  const when = expiresIn(expiresAt);
  // A list fetched before the value was stored does not know it: only a later one can say it went.
  const listAtSettle = useRef<SecretView[] | null | undefined>(undefined);
  if (settled && listAtSettle.current === undefined) listAtSettle.current = actions?.list ?? null;
  const freshList = actions?.list && actions.list !== listAtSettle.current ? actions.list : null;
  let state: "pending" | "saved" | "declined" | "revoked" | "expired" = "pending";
  if (local?.state === "revoked" || (settled && !local && freshList && !listed && when)) state = "revoked";
  else if (settled) state = when ? "saved" : "expired";
  else if (note?.outcome === "declined") state = "declined";
  const editing = (state === "pending" && !busy?.startsWith("Declin")) || reopened;
  const last4 = listed?.last4 ?? local?.view?.last4 ?? "";
  const disabled = !!readOnly || !actions || !!busy;

  const submit = async () => {
    if (!value || !actions || readOnly) return;
    const sent = value;
    // Cleared at once: the field never keeps a value that left it.
    setValue("");
    setShown(false);
    setBusy("Saving…");
    setError(null);
    try {
      const view = await actions.submit(requestId, state === "pending" ? { value: sent, ttl, scope } : { name: request.name, value: sent, ttl, scope });
      if (!mounted.current) return;
      setLocal({ state: "saved", view });
      setReopened(false);
    } catch (e) {
      if (mounted.current) setError(`Could not save ${request.name}: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      if (mounted.current) setBusy(null);
    }
  };
  const decline = async () => {
    if (reopened) return setReopened(false);
    if (!actions) return;
    setBusy("Declining…");
    setError(null);
    try { await actions.decline(requestId); } catch (e) { if (mounted.current) setError(e instanceof Error ? e.message : String(e)); } finally { if (mounted.current) setBusy(null); }
  };
  const revoke = async () => {
    if (!actions) return;
    setBusy("Revoking…");
    try { await actions.revoke(request.name); if (mounted.current) setLocal({ state: "revoked" }); } catch (e) { if (mounted.current) setError(e instanceof Error ? e.message : String(e)); } finally { if (mounted.current) setBusy(null); }
  };
  const paste = async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (!mounted.current) return;
      if (text) setValue(text.replace(/[\r\n]+$/, ""));
    } catch {
      if (!mounted.current) return;
      setError("Paste with Ctrl+V (Cmd+V) in the field.");
    }
    input.current?.focus();
  };

  const status = { pending: readOnly ? "waiting for an answer in the parent conversation" : "waiting for your answer", saved: "saved", declined: "declined", revoked: "revoked", expired: "expired" }[state];
  return (
    <div className={`perm secret-card${state === "pending" ? " waiting" : ""}`} data-state={state} role="group" aria-label={`Secret requested: ${request.label}`}>
      <div className="perm-head secret-head">
        <Key size={14} aria-hidden />
        <span className="secret-label">{request.label}</span>
        <span className="secret-status" role="status">{status}</span>
      </div>
      <div className="secret-reason">For {request.reason}. Requested by {question.agent ?? "Nav"}.</div>
      {editing && (
        <>
          <div className="secret-field">
            <input ref={input} className="mono" type={shown ? "text" : "password"} autoComplete="off" spellCheck={false} autoCapitalize="off"
              aria-label={`Value of ${request.name}`} placeholder={`Paste or type ${request.name}`} value={value} disabled={disabled}
              onChange={(e) => setValue(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); void submit(); } }} />
            <button type="button" className="icon-btn" aria-pressed={shown} aria-label={shown ? "Hide the value" : "Show the value"} title="Show or hide (the value stays on this screen)" disabled={disabled} onClick={() => setShown(!shown)}>
              {shown ? <EyeSlash size={14} /> : <Eye size={14} />}
            </button>
            <button type="button" className="icon-btn" aria-label="Paste from the clipboard" title="Paste from the clipboard" disabled={disabled} onClick={() => void paste()}>
              <ClipboardText size={14} />
            </button>
          </div>
          <div className="secret-options">
            <span className="secret-option"><span className="muted">Available</span><Segmented label="Availability" options={TTLS} value={ttl} onChange={setTtl} disabled={disabled} /></span>
            <span className="secret-option"><span className="muted">For</span><Segmented label="Scope" options={SCOPES} value={scope} onChange={setScope} disabled={disabled} /></span>
          </div>
          {value && [...value].length < 8 && <div className="secret-short">Shorter than 8 characters: it is not hidden if it shows up in a command's output.</div>}
          <div className="secret-note">Kept out of the chat. The agent receives <code>${request.name}</code>, not the value.</div>
          <div className="actions">
            <button type="button" className="btn ghost" disabled={disabled} onClick={() => void decline()}>{reopened ? "Cancel" : "Decline"}</button>
            <button type="button" className="btn" disabled={disabled || !value} onClick={() => void submit()}>Save</button>
          </div>
        </>
      )}
      {!editing && state === "saved" && (
        <div className="secret-saved">
          <code>${request.name} ••••{last4}</code>
          <span className="muted">expires {when}, {scopeText(listed?.scope ?? local?.view?.scope ?? note?.scope)}</span>
          <div className="actions">
            <button type="button" className="btn ghost" disabled={disabled} onClick={() => setReopened(true)}>Replace</button>
            <button type="button" className="btn ghost" disabled={disabled} onClick={() => void revoke()}>Revoke</button>
          </div>
        </div>
      )}
      {!editing && state !== "saved" && state !== "pending" && (
        <div className="secret-saved">
          <span className="muted">{state === "declined" ? "Nav was told and continues without it." : state === "revoked" ? `$${request.name} was revoked: commands no longer receive it.` : `$${request.name} expired: commands no longer receive it.`}</span>
          <div className="actions">
            <button type="button" className="btn ghost" disabled={disabled} onClick={() => setReopened(true)}>{state === "declined" ? "Provide anyway" : "Provide again"}</button>
          </div>
        </div>
      )}
      {busy && <div className="outcome">{busy}</div>}
      {error && <div className="composer-error" role="alert">{error}</div>}
    </div>
  );
}

/** The topbar list: the conversation's secrets, their expiry and Revoke. */
export function SecretsList({ list, error, onRevoke }: { list: SecretView[] | null; error?: string | null; onRevoke: (name: string) => void }) {
  return (
    <div className="secrets-pop" role="dialog" aria-label="Secrets">
      <div className="secrets-title">Secrets</div>
      {error && <div className="composer-error" role="alert">{error}</div>}
      {list === null ? <div className="muted">Loading…</div> : list.length === 0 ? <div className="muted">No secrets in this conversation.</div> : (
        <ul className="secrets-list">
          {list.map((s) => {
            const when = expiresIn(s.expires_at);
            return (
              <li key={s.name}>
                <span className="secrets-text">
                  <code>${s.name}{s.last4 ? ` ••••${s.last4}` : ""}</code>
                  <span className="muted">{when ? `expires ${when}` : "expired"} · {scopeText(s.scope)} · used {s.use_count}×</span>
                </span>
                <button type="button" className="btn ghost" aria-label={`Revoke ${s.name}`} onClick={() => onRevoke(s.name)}>Revoke</button>
              </li>
            );
          })}
        </ul>
      )}
      <div className="muted secrets-foot">Values stay on the server, out of the chat; the agent only sees their names.</div>
    </div>
  );
}
