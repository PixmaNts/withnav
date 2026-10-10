// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { QuestionCard } from "./QuestionCard";
import { emptyView, reduce, type Question } from "../session/reducer";

const question: Question = { type: "question", text: "Where next?", questionCallId: "ask", choices: { options: [{ label: "Develop", description: "Integration branch" }, { label: "Leave open" }], multiple: true, allow_custom: true, recommended: 0 } };
it("never preselects a recommendation, sends checked options and leaves refusals answerable", async () => {
  const host = document.createElement("div");
  const root = createRoot(host);
  const answer = vi.fn(async () => { throw new Error("Stale answer"); });
  await act(async () => root.render(<QuestionCard question={question} onAnswer={answer} />));
  const options = host.querySelectorAll<HTMLButtonElement>(".question-option");
  expect(options[0]!.getAttribute("aria-pressed")).toBe("false");
  const send = host.querySelector<HTMLButtonElement>(".actions button")!;
  expect(send.disabled).toBe(true);
  await act(async () => options[1]!.click());
  await act(async () => send.click());
  expect(answer).toHaveBeenCalledWith({ question_call_id: "ask", selected: [1] });
  expect(options[0]!.disabled).toBe(false);
  expect(host.textContent).toContain("Stale answer");
  await act(async () => root.render(<QuestionCard question={{ ...question, answer: { question_call_id: "ask", selected: [1], custom: "Until reviewed" } }} onAnswer={answer} />));
  expect(options[1]!.getAttribute("aria-pressed")).toBe("true");
  expect(options[0]!.disabled).toBe(true);
  expect(host.textContent).toContain("Until reviewed");
  await act(async () => root.unmount());
});
it("replay matches answers to their question call, not the latest card", () => {
  const event = (seq: number, kind: string, payload: object) => ({ schema_version: 1, event_id: String(seq), causation_id: null, correlation_id: null, seq, kind, payload, occurred_at: "2026-10-10", session_id: "S", actor_id: "agent:main" });
  let view = reduce(emptyView(), event(1, "turn.start", { turn_id: "T" }));
  view = reduce(view, event(2, "agent.yield", { turn_id: "T", question: question.text, question_call_id: "ask", choices: question.choices }));
  view = reduce(view, event(3, "message.received", { source: "user:http", content: "Leave open", answer: { question_call_id: "ask", selected: [1] } }));
  const turn = view.items[0]!;
  expect(turn.kind === "turn" && turn.parts[0]!.type === "question" && turn.parts[0]!.answer?.selected).toEqual([1]);
});
