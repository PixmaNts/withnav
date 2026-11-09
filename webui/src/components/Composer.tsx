import { Crew } from "./Crew";
import { ArrowUp, Image as ImageIcon, Stop, X } from "@phosphor-icons/react";
import { forwardRef, useEffect, useImperativeHandle, useMemo, useRef, useState } from "react";
import type { CommandInfo, ContextSnapshot } from "../api/types";
import { route, type ClientCommand } from "../lib/compose";
import { tokens } from "../session/reducer";
import { humanSize } from "./Images";

/** Most images one message carries; the server refuses more. */
const MAX_IMAGES = 8;
/** Largest image the server stores (`MAX_IMAGE_BYTES` in `src/session/attachments.rs`): it refuses a bigger upload. */
const MAX_IMAGE_BYTES = 3_932_160;

/** Image files among `files`: the server decides the real type from the bytes. */
function pictures(files: Iterable<File>): File[] {
  return [...files].filter((f) => f.type.startsWith("image/") || /\.(png|jpe?g|gif|webp|svg)$/i.test(f.name));
}

export interface ComposerHandle {
  focus: () => void;
  fill: (text: string) => void;
  toggleDelegate: () => void;
}

interface Props {
  agent: string;
  modelLabel: string;
  pickerOpen: boolean;
  running: boolean;
  disabled?: string;
  /** Placeholder while the conversation waits for something other than a message. */
  hint?: string;
  context: ContextSnapshot | null;
  serverCommands: CommandInfo[];
  error: string | null;
  onOpenModels: () => void;
  onToggleAgent: () => void;
  onSend: (content: import("../api/types").ContentBlock[], files?: File[]) => Promise<boolean>;
  onServerCommand: (name: string, args: string) => Promise<void>;
  onClientCommand: (name: ClientCommand, args: string) => void;
  onStop: () => void;
  onError: (message: string | null) => void;
}

const CLIENT_ROWS: CommandInfo[] = [
  { name: "new", description: "Start a new session in this workspace", args_hint: "[agent]" },
  { name: "model", description: "Choose the model and variant for this session", args_hint: "[model]" },
  { name: "skill", description: "Load a skill with your message", args_hint: "<name> [request]" },
  { name: "skills", description: "Browse the skills of this session", args_hint: "" },
  { name: "theme", description: "Choose the colour theme", args_hint: "" },
];

export const Composer = forwardRef<ComposerHandle, Props>(function Composer(props, ref) {
  const { agent, modelLabel, pickerOpen, running, disabled, context, serverCommands, error } = props;
  const [text, setText] = useState("");
  const [delegate, setDelegate] = useState(false);
  const [sending, setSending] = useState(false);
  const [slashIndex, setSlashIndex] = useState(0);
  const [slashDismissed, setSlashDismissed] = useState(false);
  const area = useRef<HTMLTextAreaElement>(null);
  const picker = useRef<HTMLInputElement>(null);
  const [files, setFiles] = useState<File[]>([]);
  const [dragging, setDragging] = useState(false);
  const addFiles = (incoming: File[]) => {
    // Refused before the upload: the server would answer 413 only once the message is sent.
    const tooLarge = incoming.filter((f) => f.size > MAX_IMAGE_BYTES);
    const accepted = incoming.filter((f) => f.size <= MAX_IMAGE_BYTES);
    // The updater stays pure (StrictMode runs it twice): the notice is raised here, once.
    const notices: string[] = [];
    if (tooLarge.length) notices.push(`${tooLarge.map((f) => f.name || "image").join(", ")}: larger than 3.75 MiB, not attached.`);
    if (files.length + accepted.length > MAX_IMAGES) notices.push(`A message carries at most ${MAX_IMAGES} images.`);
    if (notices.length) props.onError(notices.join(" "));
    if (!accepted.length) return;
    setFiles((current) => [...current, ...accepted].slice(0, MAX_IMAGES));
  };

  useImperativeHandle(ref, () => ({
    focus: () => area.current?.focus(),
    fill: (value: string) => {
      setText(value);
      requestAnimationFrame(() => {
        area.current?.focus();
        area.current?.setSelectionRange(value.length, value.length);
      });
    },
    toggleDelegate: () => setDelegate((d) => !d),
  }));

  // Grows with its content up to the CSS max-height.
  useEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
  }, [text]);

  const slash = useMemo(() => {
    if (slashDismissed || !/^\/[^\s/]*$/.test(text)) return [];
    const typed = text.slice(1).toLowerCase();
    const rows = [...CLIENT_ROWS];
    for (const c of serverCommands) if (!rows.some((r) => r.name === c.name)) rows.push(c);
    return rows.filter((r) => r.name.startsWith(typed));
  }, [text, serverCommands, slashDismissed]);
  useEffect(() => setSlashIndex(0), [slash.length]);

  const submit = async (line = text) => {
    if ((!line.trim() && !files.length) || sending || disabled) return;
    props.onError(null);
    const routed = route(line, delegate);
    if (routed.type === "refused") {
      props.onError(routed.reason);
      return;
    }
    if (files.length && routed.type !== "message") {
      props.onError("Images go with a message, not with a command.");
      return;
    }
    if (routed.type === "client") {
      setText("");
      props.onClientCommand(routed.name, routed.arguments);
      return;
    }
    setSending(true);
    // A failure keeps the draft and unlocks the composer: nothing else would ever unlock it.
    try {
      if (routed.type === "server") {
        await props.onServerCommand(routed.name, routed.arguments);
        setText("");
      } else {
        // An empty text block is dropped, unless it carries the delegation line.
        const content = line.trim() || delegate ? routed.content : routed.content.filter((b) => b.type !== "text");
        if (await props.onSend(content, files)) {
          setText("");
          setFiles([]);
          setDelegate(false);
        }
      }
    } catch (e) {
      props.onError(e instanceof Error ? e.message : String(e));
    } finally {
      setSending(false);
    }
  };

  const chooseSlash = (row: CommandInfo) => {
    setSlashDismissed(true);
    const needsArgs = row.args_hint.startsWith("<");
    if (needsArgs) {
      setText(`/${row.name} `);
      return;
    }
    void submit(`/${row.name}`);
  };

  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.nativeEvent.isComposing) return;
    if (slash.length) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        setSlashIndex((i) => (i + (e.key === "ArrowDown" ? 1 : -1) + slash.length) % slash.length);
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        e.preventDefault();
        chooseSlash(slash[slashIndex]!);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setSlashDismissed(true);
        return;
      }
    }
    // Tab on an empty message switches build / plan, as in the terminal client.
    if (e.key === "Tab" && !e.shiftKey && !text) {
      e.preventDefault();
      props.onToggleAgent();
      return;
    }
    if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
      e.preventDefault();
      void submit();
    }
  };

  const level = context ? (context.percent >= 90 ? "high" : context.percent >= 70 ? "mid" : "") : "";
  return (
    <div className="composer-wrap">
      {slash.length > 0 && (
        <div className="slash" role="listbox" aria-label="Commands">
          {slash.map((row, i) => (
            <button
              key={row.name}
              type="button"
              role="option"
              aria-selected={i === slashIndex}
              className="p-item"
              onMouseDown={(e) => e.preventDefault()}
              onMouseMove={() => setSlashIndex(i)}
              onClick={() => chooseSlash(row)}
            >
              <span className="hint">/{row.name}</span>
              <span className="lbl">
                {row.description}
                {row.args_hint && <span className="desc mono">{row.args_hint}</span>}
              </span>
              <span />
            </button>
          ))}
        </div>
      )}
      <div
        className={`composer${dragging ? " dropping" : ""}`}
        onDragOver={(e) => {
          if (disabled || !e.dataTransfer.types.includes("Files")) return;
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(e) => {
          setDragging(false);
          if (disabled || !e.dataTransfer.files.length) return;
          e.preventDefault();
          addFiles(pictures(e.dataTransfer.files));
        }}
      >
        {files.length > 0 && (
          <ul className="pending-images" aria-label="Images to send">
            {files.map((file, i) => (
              <li key={`${file.name}:${i}`} className="chip">
                <ImageIcon size={13} aria-hidden /> {file.name || "image"} · {humanSize(file.size)}
                <button type="button" className="chip-x" aria-label={`Remove ${file.name || "image"}`} onClick={() => setFiles((f) => f.filter((_, j) => j !== i))}>
                  <X size={11} />
                </button>
              </li>
            ))}
          </ul>
        )}
        <label className="sr-only" htmlFor="prompt">
          Message cognition
        </label>
        <textarea
          id="prompt"
          ref={area}
          rows={1}
          value={text}
          disabled={!!disabled}
          placeholder={disabled ?? props.hint ?? "What course would you like to set?  /skill  /command"}
          onChange={(e) => {
            setText(e.target.value);
            setSlashDismissed(false);
          }}
          onKeyDown={onKey}
          onPaste={(e) => {
            const pasted = pictures(e.clipboardData.files);
            if (!pasted.length) return;
            e.preventDefault();
            addFiles(pasted);
          }}
        />
        <input
          ref={picker}
          type="file"
          accept="image/png,image/jpeg,image/gif,image/webp,image/svg+xml"
          multiple
          hidden
          onChange={(e) => {
            addFiles(pictures(e.target.files ?? []));
            e.target.value = "";
          }}
        />
        <div className="composer-row">
          <button type="button" className="chip" onClick={() => picker.current?.click()} disabled={!!disabled} title="Attach images (or paste or drop them in the message)" aria-label="Attach images">
            <ImageIcon size={14} />
          </button>
          <button type="button" className="chip" onClick={props.onToggleAgent} disabled={!!disabled} title="Switch Nav between work and route planning (Tab on an empty message)">
            <Crew id={agent} /> ▾
          </button>
          <button
            type="button"
            className="chip model"
            aria-haspopup="dialog"
            aria-expanded={pickerOpen}
            onClick={props.onOpenModels}
            disabled={!!disabled}
            title="Model and variant for this session"
          >
            {modelLabel} ▾
          </button>
          <button
            type="button"
            className="chip"
            aria-pressed={delegate}
            onClick={() => setDelegate(!delegate)}
            disabled={!!disabled}
            title="Ask Nav to do this work itself instead of guiding you through it"
          >
            Delegate
          </button>
          {context && (
            <span className={`ctx-meter ${level}`} title={`Estimate of the next request: ${tokens(context.estimate_total)} of a ${tokens(context.window)} window (input budget ${tokens(context.input_budget)}).`}>
              {tokens(context.estimate_total)} / {tokens(context.window)} · {context.percent}%
            </span>
          )}
          {running && (
            <button type="button" className="send stop" onClick={props.onStop} title="Stop the session (the next message resumes it)">
              <Stop size={12} weight="fill" aria-hidden /> Stop
            </button>
          )}
          {(!running || text.trim()) && (
            <button
              type="button"
              className="send"
              style={running ? { marginLeft: 0 } : undefined}
              onClick={() => void submit()}
              disabled={(!text.trim() && !files.length) || sending || !!disabled}
              title={running ? "Queued: Nav reads it when this turn ends" : undefined}
            >
              <ArrowUp size={12} aria-hidden /> Send
            </button>
          )}
        </div>
      </div>
      {error && <div className="composer-error" role="alert">{error}</div>}
    </div>
  );
});
