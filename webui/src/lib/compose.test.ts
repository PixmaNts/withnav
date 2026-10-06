import { describe, expect, it } from "vitest";
import { DELEGATION_LINE, readDelegation, route } from "./compose";

describe("compose routing", () => {
  it("sends plain text and literal slashes as messages", () => {
    expect(route("hello")).toMatchObject({ type: "message", text: "hello" });
    expect(route("  /stop")).toMatchObject({ type: "message", text: "  /stop" });
    expect(route("//stop now")).toMatchObject({ type: "message", text: "/stop now" });
    expect(route("//skill x")).toMatchObject({ type: "message", text: "//skill x" });
  });

  it("keeps client commands local and sends the rest to the server", () => {
    expect(route("/model")).toEqual({ type: "client", name: "model", arguments: "" });
    expect(route("/model gw/x")).toEqual({ type: "server", name: "model", arguments: "gw/x" });
    expect(route("/variant high")).toEqual({ type: "server", name: "variant", arguments: "high" });
    expect(route("/theme now")).toEqual({ type: "refused", reason: "/theme takes no arguments" });
  });

  it("turns /skill into a message with a skill block", () => {
    expect(route("/skill git-commit tidy this")).toEqual({
      type: "message",
      text: "tidy this",
      content: [{ type: "text", text: "tidy this" }, { type: "skill", name: "git-commit" }],
    });
    expect(route("/skill")).toEqual({ type: "refused", reason: "usage: /skill <name> [request]" });
  });

  it("marks a delegated message and reads the mark back", () => {
    const routed = route("swap the parser", true);
    expect(routed).toMatchObject({ type: "message" });
    const text = (routed as { text: string }).text;
    expect(text.startsWith(DELEGATION_LINE)).toBe(true);
    expect(readDelegation(text)).toEqual({ delegated: true, text: "swap the parser" });
    expect(readDelegation("plain")).toEqual({ delegated: false, text: "plain" });
  });
});
