import { useEffect } from "react";

export interface Toast {
  id: number;
  message: string;
  /** Reverses the change by writing a new version; absent when there is nothing to undo. */
  undo?: () => Promise<void>;
  tone?: "error";
}

const SHOWN_MS = 12_000;

export function UndoToast({ toast, onDismiss }: { toast: Toast; onDismiss: () => void }) {
  useEffect(() => {
    const timer = setTimeout(onDismiss, SHOWN_MS);
    return () => clearTimeout(timer);
  }, [toast.id, onDismiss]);
  return (
    <div className="mm-toast" role="status" data-tone={toast.tone}>
      <span>{toast.message}</span>
      {toast.undo && (
        <button
          type="button"
          className="btn"
          onClick={() => {
            const undo = toast.undo!;
            onDismiss();
            void undo();
          }}
        >
          Undo
        </button>
      )}
      <button type="button" className="icon-btn" aria-label="Dismiss" onClick={onDismiss}>
        ×
      </button>
    </div>
  );
}
