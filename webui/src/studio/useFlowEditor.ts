// Editing state of the open flow. The definition is the source of truth for the canvas; the TOML
// text is the other view. Both go through POST /v1/flows/validate:
//   - a canvas edit sends the graph and gets canonical TOML back for the text view;
//   - a text edit sends the TOML and gets the parsed graph back. When the text does not parse, the
//     canvas keeps the last good definition and is locked until the text parses or is discarded.

import { useCallback, useEffect, useRef, useState } from "react";
import { envelopeOf, errorMessage, type StudioApi } from "./api";
import { acceptsRequest, acceptsValidation, applyLoad, applySave, applyValidation, documentKey, editorRequest, emptyEditor, fromDoc, type DocumentIdentity, type EditorRequest, type EditorState } from "./editorState";
import type { Content, Definition, FlowDocument, FlowSource } from "./types";

export type { DraftOrigin, EditorState } from "./editorState";

export function useFlowEditor(api: StudioApi | null, loaded: FlowDocument | undefined, identity: DocumentIdentity | null) {
  const key = documentKey(identity);
  const currentKey = useRef(key);
  currentKey.current = key;
  const [stored, setState] = useState<EditorState>({ ...emptyEditor, key });
  // Invalidate immediately on navigation, including the gap before the new document loads.
  const state = stored.key === key ? stored : { ...emptyEditor, key, generation: stored.generation + 1 };
  if (state !== stored) setState(state);
  const latest = useRef(state);
  latest.current = state;
  const update = useCallback((f: (s: EditorState) => EditorState) => {
    const next = f(latest.current);
    latest.current = next;
    setState(next);
  }, []);

  // A different flow, or a new revision of this one while nothing is being edited, replaces the state.
  useEffect(() => {
    if (!loaded || !identity || loaded.name !== identity.name || loaded.location !== identity.location) return;
    update((s) => s.key === key && s.origin === null && s.doc?.revision !== loaded.revision ? fromDoc(loaded, key, s.generation + 1) : s);
  }, [loaded, key, identity?.name, identity?.location, update]);

  const editGraph = useCallback((f: (def: Definition) => Definition) => {
    update((s) => {
      if (!s.def || s.doc?.read_only || s.tomlPending) return s;
      const def = f(s.def);
      if (def === s.def) return s;
      return { ...s, def, generation: s.generation + 1, origin: "graph", validating: false, saveError: null };
    });
  }, [update]);

  const editToml = useCallback((toml: string) => {
    update((s) => (!s.doc || s.doc.read_only || s.toml === toml ? s : { ...s, toml, generation: s.generation + 1, origin: "toml", tomlPending: true, validating: false, saveError: null }));
  }, [update]);

  const discard = useCallback(() => update((s) => (s.doc ? fromDoc(s.doc, s.key, s.generation + 1) : s)), [update]);

  // One debounce across both views; every local edit invalidates the preceding result.
  useEffect(() => {
    if (!state.origin || !api) return;
    const timer = setTimeout(async () => {
      const s = latest.current;
      if (!s.doc || !s.origin || (s.origin === "graph" && !s.def)) return;
      const request = editorRequest(s);
      update((x) => ({ ...x, validating: true, validationGeneration: request.generation }));
      try {
        const r = await api.validate(s.origin === "graph"
          ? { format: "graph", definition: s.def!, templates: s.doc.templates }
          : { format: "toml", toml: s.toml, templates: s.doc.templates });
        update((x) => applyValidation(x, request, currentKey.current, r));
      } catch (e) {
        update((x) => {
          if (!acceptsValidation(x, request, currentKey.current)) return x;
          return s.origin === "graph"
            ? { ...x, validating: false, diagnostics: envelopeOf(e)?.errors ?? [{ code: "io", message: errorMessage(e), path: "", node_id: null, line: null, column: null }] }
            : { ...x, validating: false, tomlErrors: envelopeOf(e)?.errors ?? [], tomlPending: true };
        });
      }
    }, state.origin === "graph" ? 250 : 400);
    return () => clearTimeout(timer);
  }, [state.key, state.generation, state.origin, api, update]);

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
    if (!api || !s.doc || !c || s.saving) return null;
    const request = editorRequest(s);
    update((x) => ({ ...x, saving: true, saveError: null }));
    try {
      const saved = await api.save(s.doc.name, { location: s.doc.location, revision: s.doc.revision, content: c });
      if (!acceptsRequest(latest.current, request, currentKey.current)) return null;
      update((x) => applySave(x, request, currentKey.current, saved));
      return saved;
    } catch (e) {
      const env = envelopeOf(e);
      update((x) => acceptsRequest(x, request, currentKey.current) ? { ...x, saving: false, saveError: { message: env?.error ?? errorMessage(e), reason: env?.reason ?? "", errors: env?.errors ?? [] } } : x);
      return null;
    }
  }, [api, content, update]);

  /** Adopt a document fetched or written elsewhere (enabled toggle, reopen after a stale save). */
  const request = useCallback(() => editorRequest(latest.current), []);
  const adopt = useCallback((doc: FlowDocument, token: EditorRequest) => update((s) => applyLoad(s, token, currentKey.current, doc)), [update]);

  return { state, editGraph, editToml, discard, save, source, request, adopt };
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
