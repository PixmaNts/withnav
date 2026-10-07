// Editing state of the open flow. The definition is the source of truth for the canvas; the TOML
// text is the other view. Both go through POST /v1/flows/validate:
//   - a canvas edit sends the graph and gets canonical TOML back for the text view;
//   - a text edit sends the TOML and gets the parsed graph back. When the text does not parse, the
//     canvas keeps the last good definition and is locked until the text parses or is discarded.

import { useCallback, useEffect, useRef, useState } from "react";
import { envelopeOf, errorMessage, type StudioApi } from "./api";
import type { Content, Definition, Diagnostic, FlowDocument, FlowSource } from "./types";

export type DraftOrigin = "graph" | "toml" | null;

export interface EditorState {
  doc: FlowDocument | null;
  /** What the canvas shows: the last definition that parsed. */
  def: Definition | null;
  toml: string;
  /** Errors of the definition the canvas shows. */
  diagnostics: Diagnostic[];
  /** Errors of the TOML text as typed (same as diagnostics once it parses). */
  tomlErrors: Diagnostic[];
  /** Which view holds unsaved changes; null when there are none. */
  origin: DraftOrigin;
  /** The text view has changes the canvas does not show yet (waiting, or not parsing). */
  tomlPending: boolean;
  validating: boolean;
  saving: boolean;
  saveError: { message: string; reason: string; errors: Diagnostic[] } | null;
}

const empty: EditorState = { doc: null, def: null, toml: "", diagnostics: [], tomlErrors: [], origin: null, tomlPending: false, validating: false, saving: false, saveError: null };

function fromDoc(doc: FlowDocument): EditorState {
  return { ...empty, doc, def: doc.definition, toml: doc.toml, diagnostics: doc.errors, tomlErrors: doc.errors };
}

const docKey = (doc: FlowDocument | null | undefined) => (doc ? `${doc.location}:${doc.name}` : "");

export function useFlowEditor(api: StudioApi | null, loaded: FlowDocument | undefined) {
  const [state, setState] = useState<EditorState>(empty);
  const seq = useRef(0);
  const graphRev = useRef(0);
  const [pendingGraph, setPendingGraph] = useState(0);
  const [pendingToml, setPendingToml] = useState(0);

  // A different flow, or a new revision of this one while nothing is being edited, replaces the state.
  useEffect(() => {
    if (!loaded) return;
    setState((s) => (docKey(s.doc) !== docKey(loaded) || (s.origin === null && s.doc?.revision !== loaded.revision) ? fromDoc(loaded) : s));
  }, [loaded]);

  const editGraph = useCallback((f: (def: Definition) => Definition) => {
    setState((s) => {
      if (!s.def || s.doc?.read_only || s.tomlPending) return s;
      const def = f(s.def);
      if (def === s.def) return s;
      return { ...s, def, origin: "graph", saveError: null };
    });
    graphRev.current++;
    setPendingGraph(graphRev.current);
  }, []);

  const editToml = useCallback((toml: string) => {
    setState((s) => (s.doc?.read_only ? s : { ...s, toml, origin: "toml", tomlPending: true, saveError: null }));
    setPendingToml((n) => n + 1);
  }, []);

  const discard = useCallback(() => setState((s) => (s.doc ? fromDoc(s.doc) : s)), []);

  // Canvas edits → canonical TOML and diagnostics.
  const latest = useRef(state);
  latest.current = state;
  useEffect(() => {
    if (!pendingGraph || !api) return;
    const timer = setTimeout(async () => {
      const s = latest.current;
      if (!s.def || s.origin !== "graph") return;
      const id = ++seq.current;
      setState((x) => ({ ...x, validating: true }));
      try {
        const r = await api.validate({ format: "graph", definition: s.def, templates: s.doc?.templates ?? {} });
        if (id !== seq.current) return;
        setState((x) => ({ ...x, validating: false, diagnostics: r.errors, tomlErrors: r.errors, toml: r.toml ?? x.toml }));
      } catch (e) {
        if (id !== seq.current) return;
        setState((x) => ({ ...x, validating: false, diagnostics: envelopeOf(e)?.errors ?? [{ code: "io", message: errorMessage(e), path: "", node_id: null, line: null, column: null }] }));
      }
    }, 250);
    return () => clearTimeout(timer);
  }, [pendingGraph, api]);

  // Text edits → parsed graph, or errors with the canvas left as it was.
  useEffect(() => {
    if (!pendingToml || !api) return;
    const timer = setTimeout(async () => {
      const s = latest.current;
      if (s.origin !== "toml") return;
      const id = ++seq.current;
      setState((x) => ({ ...x, validating: true }));
      try {
        const r = await api.validate({ format: "toml", toml: s.toml, templates: s.doc?.templates ?? {} });
        if (id !== seq.current) return;
        setState((x) =>
          r.definition
            ? { ...x, validating: false, def: r.definition, diagnostics: r.errors, tomlErrors: r.errors, tomlPending: false }
            : { ...x, validating: false, tomlErrors: r.errors, tomlPending: true },
        );
      } catch (e) {
        if (id !== seq.current) return;
        setState((x) => ({ ...x, validating: false, tomlErrors: envelopeOf(e)?.errors ?? [], tomlPending: true }));
      }
    }, 400);
    return () => clearTimeout(timer);
  }, [pendingToml, api]);

  /** What Save sends: the text as typed (comments kept) after a text edit, else the graph. */
  const content = useCallback((): Content | null => {
    const s = latest.current;
    if (!s.doc) return null;
    if (s.origin === "toml") return { format: "toml", toml: s.toml, templates: s.doc.templates };
    if (!s.def) return null;
    return { format: "graph", definition: s.def, templates: s.doc.templates };
  }, []);

  /** The source a sample or replay runs: the saved file, or the unsaved draft (validated, never written). */
  const source = useCallback((): FlowSource | null => {
    const s = latest.current;
    if (!s.doc) return null;
    if (s.origin === null) return { saved: { name: s.doc.name, location: s.doc.location, revision: s.doc.revision } };
    const c = content();
    return c ? { content: c } : null;
  }, [content]);

  const save = useCallback(async (): Promise<FlowDocument | null> => {
    const s = latest.current;
    const c = content();
    if (!api || !s.doc || !c) return null;
    setState((x) => ({ ...x, saving: true, saveError: null }));
    try {
      const saved = await api.save(s.doc.name, { location: s.doc.location, revision: s.doc.revision, content: c });
      seq.current++;
      setState(fromDoc(saved));
      return saved;
    } catch (e) {
      const env = envelopeOf(e);
      setState((x) => ({ ...x, saving: false, saveError: { message: env?.error ?? errorMessage(e), reason: env?.reason ?? "", errors: env?.errors ?? [] } }));
      return null;
    }
  }, [api, content]);

  /** Adopt a document fetched or written elsewhere (enabled toggle, reopen after a stale save). */
  const adopt = useCallback((doc: FlowDocument) => setState(fromDoc(doc)), []);

  return { state, editGraph, editToml, discard, save, source, adopt };
}

export type FlowEditor = ReturnType<typeof useFlowEditor>;

/**
 * Set `enabled` in the [flow] table of a TOML text without touching anything else, so the list's
 * toggle keeps the file's comments and layout.
 */
export function setEnabledInToml(toml: string, enabled: boolean): string {
  const lines = toml.split("\n");
  const start = lines.findIndex((l) => /^\s*\[flow\]\s*(#.*)?$/.test(l));
  if (start < 0) return toml;
  let end = lines.findIndex((l, i) => i > start && /^\s*\[/.test(l));
  if (end < 0) end = lines.length;
  for (let i = start + 1; i < end; i++) {
    if (/^\s*enabled\s*=/.test(lines[i]!)) {
      lines[i] = lines[i]!.replace(/=\s*(true|false)/, `= ${enabled}`);
      return lines.join("\n");
    }
  }
  const after = lines.findIndex((l, i) => i > start && i < end && /^\s*on\s*=/.test(l));
  lines.splice((after < 0 ? start : after) + 1, 0, `enabled = ${enabled}`);
  return lines.join("\n");
}
