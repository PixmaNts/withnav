import { describe, expect, it } from "vitest";
import { applyLoad, applySave, applyValidation, documentKey, editorRequest, fromDoc } from "./editorState";
import { GUARD_MUTATIONS_TOML } from "./mock/fixtures";
import { parseFlowToml } from "./mock/toml";
import type { FlowDocument, ValidateResult } from "./types";

const doc: FlowDocument = {
  name: "guard-mutations", location: "workspace", path: "/one/.withnav/flows/guard-mutations.toml",
  revision: "r1", toml: GUARD_MUTATIONS_TOML, definition: parseFlowToml(GUARD_MUTATIONS_TOML).definition,
  templates: {}, errors: [], saved_since_reload: false, read_only: false,
};
const key = documentKey({ workspace: "/one", name: doc.name, location: doc.location });
const result: ValidateResult = { valid: true, definition: doc.definition, toml: "canonical", templates: {}, errors: [] };
const draft = () => ({ ...fromDoc(doc, key, 1), origin: "toml" as const, toml: "draft", tomlPending: true, validating: true, validationGeneration: 1 });

describe("async editor responses", () => {
  it.each([
    { workspace: "/one", name: "another-flow", location: "workspace" as const },
    { workspace: "/two", name: doc.name, location: doc.location },
    { workspace: "/one", name: doc.name, location: "global" as const },
  ])("drops validation after switching to $workspace/$name/$location", (identity) => {
    const old = draft();
    const nextKey = documentKey(identity);
    const next = fromDoc({ ...doc, name: identity.name, location: identity.location }, nextKey, 2);
    expect(applyValidation(next, editorRequest(old), nextKey, result)).toBe(next);
    // Navigation invalidates even before the replacement document has arrived.
    expect(applyValidation(old, editorRequest(old), nextKey, result)).toBe(old);
  });

  it.each(["graph", "toml"] as const)("keeps a newer %s draft and the saved revision", (origin) => {
    const started = { ...draft(), origin, saving: true };
    const editedDefinition = { ...doc.definition!, flow: { ...doc.definition!.flow, enabled: false } };
    const edited = { ...started, generation: 2, def: editedDefinition, toml: "newer local text" };
    const saved = { ...doc, revision: "r2", saved_since_reload: true };
    const next = applySave(edited, editorRequest(started), key, saved);
    expect(next.doc).toBe(saved);
    expect(next.doc?.revision).toBe("r2");
    expect(next.def).toBe(editedDefinition);
    expect(next.toml).toBe("newer local text");
    expect(next.origin).toBe(origin);
    expect(next.tomlPending).toBe(edited.tomlPending);
    expect(next.saving).toBe(false);
  });

  it("ignores validation for an older generation, even before the new debounce fires", () => {
    const old = draft();
    const edited = { ...old, generation: 2, toml: "new draft" };
    expect(applyValidation(edited, editorRequest(old), key, result)).toBe(edited);
    const validating = { ...edited, validationGeneration: 2 };
    expect(applyValidation(validating, editorRequest(old), key, result)).toBe(validating);
    expect(applyValidation(validating, editorRequest(validating), key, result)).toMatchObject({ def: doc.definition, tomlPending: false, validating: false });
  });

  it("adopts a save with no intervening edits and invalidates pending validation", () => {
    const started = draft();
    const saved = { ...doc, revision: "r2" };
    const next = applySave(started, editorRequest(started), key, saved);
    expect(next).toMatchObject({ doc: saved, toml: saved.toml, origin: null, saving: false });
    expect(applyValidation(next, editorRequest(started), key, result)).toBe(next);
  });

  it("drops save and load responses after switching documents", () => {
    const started = draft();
    const nextKey = documentKey({ workspace: "/two", name: doc.name, location: doc.location });
    const next = fromDoc(doc, nextKey, 2);
    expect(applySave(next, editorRequest(started), nextKey, doc)).toBe(next);
    expect(applyLoad(next, editorRequest(started), nextKey, doc)).toBe(next);
  });

  it("does not let a late load discard edits made since it started", () => {
    const started = draft();
    const edited = { ...started, generation: 2, toml: "newer draft" };
    expect(applyLoad(edited, editorRequest(started), key, doc)).toBe(edited);
    expect(applyLoad(started, editorRequest(started), key, doc)).toMatchObject({ doc, origin: null });
  });
});
