import { useState } from "react";
import type { AskAnswer, Question } from "../session/reducer";
import { Markdown } from "./Markdown";

/** A suggestion is a hint, not the Captain's selection. Only an explicit action sends it. */
export function QuestionCard({ question, onAnswer, readOnly }: {
  question: Question; onAnswer: (answer: AskAnswer) => Promise<void>; readOnly?: boolean;
}) {
  const [selected, setSelected] = useState<number[]>([]);
  const [custom, setCustom] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const choices = question.choices!;
  const answered = !!question.answer;
  const disabled = sending || answered || !!readOnly;
  const picked = question.answer?.selected ?? selected;
  const submit = async (indices: number[]) => {
    setSending(true);
    setError(null);
    try {
      await onAnswer({ question_call_id: question.questionCallId!, selected: indices, ...(custom.trim() ? { custom: custom.trim() } : {}) });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSending(false);
    }
  };
  return <div className={`perm question-card${answered ? "" : " waiting"}`} role="group" aria-label={choices.header ?? "Question"}>
    <div className="perm-head">{choices.header ?? "Question"}{answered && <span className="muted"> · Answered</span>}</div>
    <div className="perm-reason"><Markdown text={question.text} /></div>
    <div className="question-options">
      {choices.options.map((option, index) => <button type="button" key={index}
        className={`btn ghost question-option${picked.includes(index) ? " selected" : ""}`}
        disabled={disabled} aria-pressed={picked.includes(index)}
        onClick={() => {
          if (choices.multiple) setSelected(picked.includes(index) ? picked.filter((i) => i !== index) : [...picked, index].sort((a, b) => a - b));
          else { setSelected([index]); void submit([index]); }
        }}>
        {choices.multiple && <span aria-hidden className="question-check">{picked.includes(index) ? "☑" : "☐"}</span>}
        <span><span className="question-option-label">{option.label}{choices.recommended === index && <span className="muted"> · Recommended</span>}</span>
          {option.description && <span className="question-description">{option.description}</span>}</span>
      </button>)}
    </div>
    {choices.allow_custom && !answered && <label className="question-custom">Other answer…
      <input value={custom} disabled={disabled} onChange={(e) => setCustom(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && custom.trim()) { e.preventDefault(); void submit(selected); } }} />
    </label>}
    {!answered && !readOnly && (choices.multiple || choices.allow_custom) && <div className="actions">
      <button type="button" className="btn" disabled={disabled || (!selected.length && !custom.trim())} onClick={() => submit(selected)}>Send</button>
      {sending && <span className="muted">Sending…</span>}
    </div>}
    {question.answer?.custom && <div className="outcome">{question.answer.custom}</div>}
    {readOnly && !answered && <div className="outcome">Waiting for an answer in the parent conversation.</div>}
    {error && <div className="composer-error" role="alert">{error}</div>}
  </div>;
}
