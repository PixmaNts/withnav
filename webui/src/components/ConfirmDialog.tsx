import { useEffect, useId, useRef, type ReactNode } from "react";

/**
 * The shell of a confirmation: dim backdrop, a titled alert dialog, Cancel and one action.
 * The destructive variant puts the focus on Cancel, so Enter alone never deletes anything.
 */
export function ConfirmDialog({
  title,
  icon,
  danger,
  children,
  error,
  busy,
  confirmLabel,
  onConfirm,
  onDismiss,
}: {
  title: ReactNode;
  icon: ReactNode;
  danger: boolean;
  children: ReactNode;
  error: string | null;
  busy: boolean;
  confirmLabel: string;
  onConfirm: () => void;
  onDismiss: () => void;
}) {
  const cancel = useRef<HTMLButtonElement>(null);
  const confirm = useRef<HTMLButtonElement>(null);
  const before = useRef<Element | null>(typeof document === "undefined" ? null : document.activeElement);
  const titleId = useId();
  const bodyId = useId();

  useEffect(() => {
    (danger ? cancel : confirm).current?.focus();
  }, [danger]);

  const dismiss = () => {
    if (busy) return;
    onDismiss();
    const back = before.current;
    if (back instanceof HTMLElement && back.isConnected) requestAnimationFrame(() => back.focus());
  };

  return (
    <div className="backdrop" onMouseDown={dismiss}>
      <div
        className="confirm"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={bodyId}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            dismiss();
          } else if (e.key === "Tab") {
            // Two buttons: keep focus inside the dialog.
            const order = [cancel.current, confirm.current].filter((b): b is HTMLButtonElement => !!b && !b.disabled);
            if (!order.length) return;
            const at = order.indexOf(document.activeElement as HTMLButtonElement);
            e.preventDefault();
            order[(at + (e.shiftKey ? -1 : 1) + order.length) % order.length]!.focus();
          }
        }}
      >
        <h2 id={titleId} className={danger ? "danger" : undefined}>
          {icon}
          {title}
        </h2>
        <div id={bodyId} className="confirm-body">
          {children}
        </div>
        {error && (
          <div className="confirm-error" role="alert">
            {error}
          </div>
        )}
        <div className="confirm-actions">
          <button ref={cancel} type="button" className="btn ghost" onClick={dismiss} disabled={busy}>
            Cancel
          </button>
          <button ref={confirm} type="button" className={`btn${danger ? " danger solid" : ""}`} onClick={onConfirm} disabled={busy}>
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
