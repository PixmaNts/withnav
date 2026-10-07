import { CaretLeft, MagnifyingGlass } from "@phosphor-icons/react";
import { useEffect, useMemo, useRef, useState } from "react";
import { ranked } from "../lib/search";

export interface PaletteCommand {
  id: string;
  group: string;
  label: string;
  /** Secondary text, searched too. */
  description?: string;
  hint?: string;
  disabled?: string;
  /** Run and close, or open a sub-list. */
  run?: () => void;
  page?: () => PalettePage;
}

export interface PalettePage {
  title: string;
  placeholder: string;
  items: PaletteCommand[];
  empty?: string;
}

/** The ⌘K palette: centred, dim backdrop, grouped rows with key hints. */
export function CommandPalette({ root, onClose }: { root: PalettePage; onClose: () => void }) {
  const [stack, setStack] = useState<PalettePage[]>([root]);
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const page = stack[stack.length - 1]!;
  const items = useMemo(() => ranked(page.items, query, (c) => c.label, (c) => `${c.group} ${c.description ?? ""} ${c.hint ?? ""}`), [page, query]);

  useEffect(() => input.current?.focus(), [stack.length]);
  useEffect(() => setIndex(0), [query, stack.length]);
  useEffect(() => {
    list.current?.querySelector(`[data-index="${index}"]`)?.scrollIntoView({ block: "nearest" });
  }, [index]);

  const choose = (item: PaletteCommand | undefined) => {
    if (!item || item.disabled) return;
    if (item.page) {
      setStack((s) => [...s, item.page!()]);
      setQuery("");
      return;
    }
    onClose();
    item.run?.();
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (query) setQuery("");
      else if (stack.length > 1) setStack((s) => s.slice(0, -1));
      else onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (items.length) setIndex((i) => (i + 1) % items.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (items.length) setIndex((i) => (i - 1 + items.length) % items.length);
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(items[index]);
    } else if (e.key === "Backspace" && !query && stack.length > 1) {
      e.preventDefault();
      setStack((s) => s.slice(0, -1));
    }
  };

  return (
    <div className="backdrop" onMouseDown={onClose}>
      <div className="palette" role="dialog" aria-modal="true" aria-label={page.title} onMouseDown={(e) => e.stopPropagation()} onKeyDown={onKey}>
        <div className="palette-search">
          {stack.length > 1 ? (
            <button type="button" className="icon-btn" aria-label="Back" onClick={() => setStack((s) => s.slice(0, -1))}>
              <CaretLeft size={14} />
            </button>
          ) : (
            <MagnifyingGlass size={15} aria-hidden />
          )}
          {stack.length > 1 && <span>{page.title}</span>}
          <input
            ref={input}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={page.placeholder}
            aria-label={page.placeholder}
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-list"
            aria-activedescendant={items[index] ? `pal-${index}` : undefined}
          />
          <span className="esc">esc</span>
        </div>
        <div className="palette-list" id="palette-list" role="listbox" ref={list}>
          {!items.length && <div className="picker-empty">{query ? "Nothing matches." : page.empty ?? "Nothing here."}</div>}
          {items.map((item, i) => (
            <button
              key={item.id}
              id={`pal-${i}`}
              data-index={i}
              type="button"
              role="option"
              aria-selected={i === index}
              aria-disabled={!!item.disabled}
              className="p-item"
              style={item.disabled ? { opacity: 0.5, cursor: "default" } : undefined}
              title={item.disabled}
              onMouseMove={() => setIndex(i)}
              onClick={() => choose(item)}
            >
              <span className="group">{item.group}</span>
              <span className="lbl">
                {item.label}
                {item.description && <span className="desc">{item.description}</span>}
              </span>
              <span className="hint">{item.hint}</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
