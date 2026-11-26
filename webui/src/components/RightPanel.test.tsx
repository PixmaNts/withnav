// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MemoryItem, SessionLedger } from "../api/types";
import type { Chat } from "../session/useChat";
import { RightPanel } from "./RightPanel";

vi.mock("../api/queries", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api/queries")>()),
  useLedger: vi.fn(),
  useSessionMemory: vi.fn(),
}));
import { useLedger, useSessionMemory } from "../api/queries";

const ledger = (over: Partial<SessionLedger> = {}): SessionLedger => ({
  through_seq: 10,
  turns: { count: 0, last: null },
  files_touched: [],
  commands: [],
  repeated_failures: [],
  open_questions: [],
  compactions: 0,
  goal: null,
  goal_check: null,
  ...over,
});

const memoryItem = (id: string, title: string, scope: MemoryItem["scope"]): MemoryItem =>
  ({ id, kind: "decision", scope, scope_ref: "", status: "active", current_version: 1, pinned: false, created_at: "", updated_at: "", current: { title, version: 1 } }) as MemoryItem;

const goal = { revision: 3, objective: "Fix the clippy warnings and get Windows CI green.", criteria: ["clippy is clean", "Windows CI passes", "docs updated"], status: "active" as const };

let container: HTMLDivElement;
let root: Root;
let epoch = 0;

function show(data: SessionLedger | undefined, memory: MemoryItem[] | undefined = [], sessionId: string | null = "S1", client = new QueryClient()) {
  vi.mocked(useLedger).mockReturnValue({ data, isError: data === undefined && !!sessionId } as never);
  vi.mocked(useSessionMemory).mockReturnValue({ data: memory, isError: false } as never);
  const chat = { summary: sessionId ? { session_id: sessionId } : null, view: { sessionEpoch: epoch, children: {} }, agents: null } as unknown as Chat;
  act(() =>
    root.render(
      <QueryClientProvider client={client}>
        <RightPanel tab="session" setTab={vi.fn()} workspace="/ws" chat={chat} onOpenChild={vi.fn()} />
      </QueryClientProvider>,
    ),
  );
  return client;
}
const text = () => container.textContent ?? "";

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.clearAllMocks();
  epoch = 0;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the Session tab", () => {
  it("is offered beside the others and says nothing is there before the session starts", () => {
    show(undefined, undefined, null);
    expect([...container.querySelectorAll('[role="tab"]')].map((t) => t.textContent)).toContain("Session");
    expect(text()).toContain("starts");
    expect(useLedger).toHaveBeenCalledWith(null);
  });

  it("tells the Captain how to set a goal when there is none", () => {
    show(ledger());
    expect(text()).toContain("No goal for this session");
    expect(text()).toContain("/goal");
  });

  it("shows the goal's objective, revision and status, and each criterion with the status of its last check", () => {
    show(
      ledger({
        goal,
        goal_check: {
          revision: 3,
          turn_id: "01KB0000000000000000000000",
          note_sent: false,
          claims: [],
          statuses: [
            { index: 1, status: "supported", evidence: "cargo clippy exited 0" },
            { index: 2, status: "contradicted", evidence: "cargo test exited 101" },
          ],
        },
      }),
    );
    expect(text()).toContain("Fix the clippy warnings");
    expect(text()).toContain("revision 3");
    expect(text()).toContain("active");
    const rows = [...container.querySelectorAll("[data-criterion]")] as HTMLElement[];
    expect(rows.map((r) => [r.dataset.criterion, r.dataset.status])).toEqual([
      ["1", "supported"],
      ["2", "contradicted"],
      ["3", "not_checked"],
    ]);
    expect(rows[0]!.textContent).toContain("clippy is clean");
    expect(rows[0]!.textContent).toContain("supported");
    expect(rows[1]!.textContent).toContain("contradicted");
    expect(rows[2]!.textContent).toContain("not checked");
    expect(rows[0]!.getAttribute("title")).toBe("cargo clippy exited 0");
    expect(rows[1]!.getAttribute("title")).toBe("cargo test exited 101");
    expect(rows[2]!.getAttribute("title")).toBeNull();
    expect(text()).toContain("last checked");
  });

  it("treats every criterion as not checked when the last check judged another revision, and says no check ran", () => {
    show(ledger({ goal, goal_check: { revision: 2, turn_id: "T", note_sent: false, claims: [], statuses: [{ index: 1, status: "supported", evidence: "" }] } }));
    const rows = [...container.querySelectorAll("[data-criterion]")] as HTMLElement[];
    expect(rows.map((r) => r.dataset.status)).toEqual(["not_checked", "not_checked", "not_checked"]);
    expect(text()).not.toContain("last checked");
    show(ledger({ goal }));
    expect(text()).toContain("not checked yet");
  });

  it("lists the claims of the agent's last turn; an unverified one is not called false", () => {
    show(
      ledger({
        goal,
        goal_check: {
          revision: 3,
          turn_id: "T",
          note_sent: false,
          statuses: [],
          claims: [
            { claim: "tests pass", status: "supported", evidence: "cargo test exited 0" },
            { claim: "the build passes", status: "contradicted", evidence: "cargo build exited 101" },
            { claim: "committed", status: "unverified", evidence: "" },
          ],
        },
      }),
    );
    const rows = [...container.querySelectorAll("[data-claim]")] as HTMLElement[];
    expect(rows.map((r) => [r.textContent, r.dataset.status])).toEqual([
      ["tests passsupported", "supported"],
      ["the build passescontradicted", "contradicted"],
      ["committedunverified", "unverified"],
    ]);
    expect(rows[0]!.getAttribute("title")).toBe("cargo test exited 0");
    expect(text()).not.toContain("false");
  });

  it("marks a paused goal", () => {
    show(ledger({ goal: { ...goal, status: "paused" } }));
    expect(text()).toContain("paused");
  });

  it("counts the turns, the files with the last three, the repeated failures with the worst and the open questions", () => {
    show(
      ledger({
        turns: { count: 14, last: { turn_id: "T", outcome: "done" } },
        files_touched: ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"].map((path) => ({ path, changes: 1, last_turn: 1 })),
        repeated_failures: [
          { command: "cargo test", count: 3, last_error: "exit 101" },
          { command: "make", count: 2, last_error: "no rule" },
        ],
        open_questions: [{ kind: "ask", id: "q1", title: "Does the path fixture have to change?" }],
      }),
    );
    const value = (label: string) => container.querySelector(`[data-ledger="${label}"]`)?.textContent;
    expect(value("turns")).toBe("14");
    expect(value("files")).toBe("6");
    expect(value("failures")).toBe("2");
    expect(value("questions")).toBe("1");
    expect([...container.querySelectorAll("[data-file]")].map((f) => f.textContent)).toEqual(["a.rs", "b.rs", "c.rs"]);
    expect(text()).toContain("cargo test");
    expect(text()).toContain("3×");
    expect(text()).not.toContain("make");
    expect(text()).toContain("Does the path fixture have to change?");
  });

  it("shows zeroes for an empty ledger, not blanks", () => {
    show(ledger());
    expect(container.querySelector('[data-ledger="turns"]')?.textContent).toBe("0");
    expect(container.querySelector('[data-ledger="files"]')?.textContent).toBe("0");
    expect(container.querySelector('[data-ledger="failures"]')?.textContent).toBe("0");
    expect(container.querySelector('[data-ledger="questions"]')?.textContent).toBe("0");
  });

  it("lists the memory items the session is eligible for, by title with their scope", () => {
    show(ledger(), [memoryItem("m1", "Use conventional commits", "project"), memoryItem("m2", "Reply in French", "global")]);
    const rows = [...container.querySelectorAll("[data-memory]")] as HTMLElement[];
    expect(rows.map((r) => [r.dataset.memory, r.textContent])).toEqual([
      ["m1", "Use conventional commits · project"],
      ["m2", "Reply in French · global"],
    ]);
    expect(container.querySelector('a[href^="#/memory"]')).not.toBeNull();
    expect(useSessionMemory).toHaveBeenCalledWith("S1");
  });

  it("says when no memory item applies, and when the ledger cannot be read", () => {
    show(ledger(), []);
    expect(text()).toContain("No memory item applies");
    show(undefined);
    expect(text()).toContain("could not be read");
  });

  it("reads the ledger and the memory again when the stream says the session changed, and not before", () => {
    const client = new QueryClient();
    show(ledger(), [], "S1", client);
    const invalidate = vi.spyOn(client, "invalidateQueries");
    show(ledger(), [], "S1", client);
    expect(invalidate).not.toHaveBeenCalled();
    epoch = 1;
    show(ledger(), [], "S1", client);
    expect(invalidate.mock.calls.map(([filter]) => filter?.queryKey)).toEqual([["ledger", "S1"], ["memory", "session", "S1"]]);
  });
});
