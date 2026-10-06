import { describe, expect, it } from "vitest";
import { shortName } from "./ModelPicker";

// Same cases as `src/tui/catalog.rs` (short_name tests), so the three pickers agree.
describe("model short name", () => {
  const m = (name: string, gateway: string, provider_group: string) => ({ name, gateway, provider_group });
  it("drops the gateway and provider the headers show", () => {
    expect(shortName(m("omniroute/agy/claude-opus", "omniroute", "agy"))).toBe("claude-opus");
    expect(shortName(m("omniroute/agy/vendor/claude", "omniroute", "agy"))).toBe("vendor/claude");
  });
  it("drops only the gateway of a direct gateway", () => {
    expect(shortName(m("direct/solo", "direct", "direct"))).toBe("solo");
  });
  it("falls back to the full name when the prefix does not match", () => {
    for (const [name, gateway, group] of [
      ["other/agy/claude", "omniroute", "agy"],
      ["omniroute/zzz/claude", "omniroute", "agy"],
      ["omniroute/agyclaude", "omniroute", "agy"],
      ["omniroute/agy/", "omniroute", "agy"],
      ["direct", "direct", "direct"],
      ["directx/solo", "direct", "direct"],
    ] as const)
      expect(shortName(m(name, gateway, group))).toBe(name);
  });
});
