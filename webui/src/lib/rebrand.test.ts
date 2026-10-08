import { expect, it, vi } from "vitest";
import { migratedValue } from "./storage";
import { crewId, crewLabel } from "../components/Crew";
import { authorOf } from "../session/reducer";
import { parseFlowToml } from "../studio/mock/toml";
import { GUARD_MUTATIONS_TOML } from "../studio/mock/fixtures";

it.each(["theme", "token", "expanded-workspaces"])("migrates %s on first read", (suffix) => {
  const values = new Map([[`navai.${suffix}`, "saved"]]);
  const storage = { getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => { values.set(key, value); }, removeItem: (key: string) => { values.delete(key); } };
  expect(migratedValue(storage, `withnav.${suffix}`, `navai.${suffix}`)).toBe("saved");
  expect(values.get(`withnav.${suffix}`)).toBe("saved");
  expect(values.has(`navai.${suffix}`)).toBe(false);
});
it.each(["driver", "captain", "captain:main"])("recognizes %s as Captain", (source) => expect(authorOf(source)).toBe("captain"));
it.each([["build", "nav"], ["plan", "nav"], ["explore", "vigie"], ["librarian", "vigie"], ["review", "lux"], ["oracle", "lux"], ["explorer", "mousse"], ["driver", "captain"]])("maps %s to %s", (old, current) => expect(crewId(old)).toBe(current));
it.each(["nav", "vigie", "lux", "mousse", "cartographe", "sondeur", "captain"])("labels %s with a role", (id) => expect(crewLabel(id)).toContain(" · The "));
it("reads ask_driver as ask_captain", () => {
  const old = GUARD_MUTATIONS_TOML.replaceAll("ask_captain", "ask_driver");
  expect(JSON.stringify(parseFlowToml(old).definition)).not.toContain("ask_driver");
  expect(JSON.stringify(parseFlowToml(old).definition)).toContain("ask_captain");
});
it("prefers new env names and warns once for old env fallbacks", async () => {
  vi.resetModules();
  const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
  const { devEnv } = await import("../../dev/legacyConfig");
  expect(devEnv("TOKEN", { WITHNAV_TOKEN: "new", NAVAI_TOKEN: "old" })).toBe("new");
  expect(warning).not.toHaveBeenCalled();
  expect(devEnv("SERVER", { NAVAI_SERVER: "old" })).toBe("old");
  expect(devEnv("TOKEN", { NAVAI_TOKEN: "old" })).toBe("old");
  expect(warning).toHaveBeenCalledTimes(1);
  warning.mockRestore();
});
it("warns once for the old config directory", async () => {
  vi.resetModules();
  const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
  const { devConfigDir } = await import("../../dev/legacyConfig");
  expect(devConfigDir({}, "/home/test", () => true)).toBe("/home/test/.config/withnav");
  expect(warning).not.toHaveBeenCalled();
  expect(devConfigDir({}, "/home/test", () => false)).toBe("/home/test/.config/navai");
  expect(devConfigDir({}, "/home/test", () => false)).toBe("/home/test/.config/navai");
  expect(warning).toHaveBeenCalledTimes(1);
  warning.mockRestore();
});
