import { ArrowUp, Stop } from "@phosphor-icons/react";
import { forwardRef, useEffect, useImperativeHandle, useMemo, useRef, useState } from "react";
import type { CommandInfo, ContextSnapshot } from "../api/types";
import { route, type ClientCommand } from "../lib/compose";
import { tokens } from "../session/reducer";

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
  context: ContextSnapshot | null;
  serverCommands: CommandInfo[];
  error: string | null;
  onOpenModels: () => void;
  onToggleAgent: () => void;
  onSend: (content: import("../api/types").ContentBlock[]) => Promise<boolean>;
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
    if (!line.trim() || sending || disabled) return;
    props.onError(null);
    const routed = route(line, delegate);
    if (routed.type === "refused") {
      props.onError(routed.reason);
      return;
    }
    if (routed.type === "client") {
      setText("");
      props.onClientCommand(routed.name, routed.arguments);
      return;
    }
    setSending(true);
    if (routed.type === "server") {
      await props.onServerCommand(routed.name, routed.arguments);
      setText("");
    } else if (await props.onSend(routed.content)) {
      setText("");
      setDelegate(false);
    }
    setSending(false);
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
      <div className="composer">
        <label className="sr-only" htmlFor="prompt">
          Message cognition
        </label>
        <textarea
          id="prompt"
          ref={area}
          rows={1}
          value={text}
          disabled={!!disabled}
          placeholder={disabled ?? "Message cognition…  /skill  /command"}
          onChange={(e) => {
            setText(e.target.value);
            setSlashDismissed(false);
          }}
          onKeyDown={onKey}
        />
        <div className="composer-row">
          <button type="button" className="chip" onClick={props.onToggleAgent} disabled={!!disabled} title="Switch build / plan (Tab on an empty message)">
            {agent} ▾
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
            title="Ask the cognition to do this work itself instead of guiding you through it"
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
              disabled={!text.trim() || sending || !!disabled}
              title={running ? "Queued: the cognition reads it when this turn ends" : undefined}
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
