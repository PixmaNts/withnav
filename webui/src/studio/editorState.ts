import type { Definition, Diagnostic, FlowDocument, Location, ValidateResult } from "./types";

export type DraftOrigin = "graph" | "toml" | null;
export interface DocumentIdentity { workspace: string; name: string; location: Location }
export interface EditorRequest { key: string; generation: number }
export interface EditorState {
  key: string;
  generation: number;
  validationGeneration: number | null;
  doc: FlowDocument | null;
  /** The last definition that parsed, even while the TOML draft does not parse. */
  def: Definition | null;
  toml: string;
  diagnostics: Diagnostic[];
  tomlErrors: Diagnostic[];
  origin: DraftOrigin;
  tomlPending: boolean;
  validating: boolean;
  saving: boolean;
  saveError: { message: string; reason: string; errors: Diagnostic[] } | null;
}

export const documentKey = (identity: DocumentIdentity | null) => identity ? JSON.stringify([identity.workspace, identity.name, identity.location]) : "";
export const emptyEditor: EditorState = { key: "", generation: 0, validationGeneration: null, doc: null, def: null, toml: "", diagnostics: [], tomlErrors: [], origin: null, tomlPending: false, validating: false, saving: false, saveError: null };
export const editorRequest = (state: EditorState): EditorRequest => ({ key: state.key, generation: state.generation });

export function fromDoc(doc: FlowDocument, key: string, generation: number): EditorState {
  doc = JSON.parse(JSON.stringify(doc), (key, value) => key === "action" && value === "ask_driver" ? "ask_captain" : value);
  doc.toml = doc.toml.replace(/(action\s*=\s*["'])ask_driver(["'])/g, "$1ask_captain$2");
  return { ...emptyEditor, key, generation, doc, def: doc.definition, toml: doc.toml, diagnostics: doc.errors, tomlErrors: doc.errors };
}

export function acceptsRequest(state: EditorState, request: EditorRequest, currentKey: string): boolean {
  return request.key === currentKey && state.key === request.key;
}

export function acceptsValidation(state: EditorState, request: EditorRequest, currentKey: string): boolean {
  return acceptsRequest(state, request, currentKey) && state.generation === request.generation && state.validationGeneration === request.generation;
}

export function applyValidation(state: EditorState, request: EditorRequest, currentKey: string, result: ValidateResult): EditorState {
  if (!acceptsValidation(state, request, currentKey)) return state;
  if (state.origin === "graph") return { ...state, validating: false, diagnostics: result.errors, tomlErrors: result.errors, toml: result.toml ?? state.toml };
  return result.definition
    ? { ...state, validating: false, def: result.definition, diagnostics: result.errors, tomlErrors: result.errors, tomlPending: false }
    : { ...state, validating: false, tomlErrors: result.errors, tomlPending: true };
}

function matchesDocument(doc: FlowDocument, key: string): boolean {
  const [, name, location] = JSON.parse(key) as string[];
  return doc.name === name && doc.location === location;
}

export function applySave(state: EditorState, request: EditorRequest, currentKey: string, saved: FlowDocument): EditorState {
  if (!acceptsRequest(state, request, currentKey) || !matchesDocument(saved, request.key)) return state;
  // The file is the new baseline, not necessarily the draft currently being edited.
  if (state.generation !== request.generation) return { ...state, doc: saved, origin: state.origin ?? (state.def ? "graph" : "toml"), saving: false, saveError: null };
  return fromDoc(saved, state.key, state.generation + 1);
}

export function applyLoad(state: EditorState, request: EditorRequest, currentKey: string, doc: FlowDocument): EditorState {
  if (!acceptsRequest(state, request, currentKey) || state.generation !== request.generation || !matchesDocument(doc, request.key)) return state;
  return fromDoc(doc, state.key, state.generation + 1);
}
