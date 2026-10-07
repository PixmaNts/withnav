import { ArrowClockwise, Clock, Lightning, WarningCircle } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import type { SettingsBody } from "../api/types";
import { appliesText, fieldOf } from "../lib/settings";

/** When a change to `key` takes effect, as the server reports it: a quiet line near the control. */
export function Applies({ body, keyName, className }: { body: SettingsBody; keyName: string; className?: string }) {
  const applies = fieldOf(body, keyName)?.applies;
  const text = appliesText(applies);
  if (!text) return null;
  const icon = applies === "restart" ? <ArrowClockwise size={11} aria-hidden /> : applies === "live" ? <Lightning size={11} aria-hidden /> : <Clock size={11} aria-hidden />;
  return (
    <span className={`st-applies ${applies}${className ? ` ${className}` : ""}`} title={applies === "restart" ? "Navai builds its layers when it starts: the running server keeps the current model until it restarts." : applies === "new_sessions" ? "Sessions already open keep the value they started with." : applies === "live" ? "Other open WebUI tabs and terminal clients pick it up when they next start." : undefined}>
      {icon} {text}
    </span>
  );
}

/** Section title, one-line description and the file it lives in. */
export function Head({ title, file, children }: { title: string; file: string; children: ReactNode }) {
  return (
    <>
      <h1 className="st-title">{title}</h1>
      <div className="st-desc">
        {children} <span className="mono">{file}</span>
      </div>
    </>
  );
}

export function Field({ label, note, wide, error, children }: { label: ReactNode; note?: ReactNode; wide?: boolean; error?: string | null; children: ReactNode }) {
  return (
    <div className={wide ? "st-field wide" : "st-field"}>
      <div className="st-label">{label}</div>
      {children}
      {error ? <FieldError message={error} /> : note ? <div className="st-note">{note}</div> : null}
    </div>
  );
}

/** The server's message for a refused or invalid value, right under its control. */
export function FieldError({ message }: { message: string }) {
  return (
    <div className="st-error" role="alert">
      <WarningCircle size={13} aria-hidden /> {message}
    </div>
  );
}

/** A choice button: accent border and tint when on (the design's "pill"). */
export function Pill({ on, onClick, disabled, title, mono, children }: { on: boolean; onClick?: () => void; disabled?: boolean; title?: string; mono?: boolean; children: ReactNode }) {
  return (
    <button type="button" className={`st-pillbtn${mono ? " mono" : ""}`} aria-pressed={on} onClick={onClick} disabled={disabled} title={title}>
      {children}
    </button>
  );
}
