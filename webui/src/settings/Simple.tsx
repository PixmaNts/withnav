import { Compass, Folder, Gauge, User, X } from "@phosphor-icons/react";
import { useState } from "react";
import { asBool, asInt, asPaths, asTheme, fieldOf, WATCHDOG_PRESETS } from "../lib/settings";
import { THEMES } from "../theme/themes";
import { isSet, problem, shown, type SectionProps } from "./context";
import { Applies, FieldError, Head, Pill } from "./ui";

// ---------- Permissions ----------

export function Permissions(p: SectionProps) {
  const review = asBool(shown(p, "permissions.automatic_review"), true);
  const reviewError = problem(p, "permissions.automatic_review");
  const dirs = asPaths(shown(p, "permissions.scratch_dirs"));
  const defaults = asPaths(fieldOf(p.body, "permissions.scratch_dirs")?.default);
  const dirsSet = isSet(p, "permissions.scratch_dirs");
  const dirsError = problem(p, "permissions.scratch_dirs");
  const [draft, setDraft] = useState("");
  const [local, setLocal] = useState<string | null>(null);

  const add = () => {
    const path = draft.trim().replace(/(.)\/+$/, "$1");
    if (!path) return;
    if (!path.startsWith("/")) return setLocal("Use an absolute path, starting with /.");
    if (dirs.includes(path)) return setLocal("This directory is already in the list.");
    setLocal(null);
    setDraft("");
    p.save.save("permissions.scratch_dirs", [...dirs, path]);
  };

  return (
    <>
      <Head title="Permissions" file="config.json → permissions">
        How mutating tool calls are cleared.
      </Head>
      <div className="st-stack">
        {p.visible("perm:review") && (
          <div className="st-card">
            <div className="st-card-top">
              <div className="st-card-title">Automatic review</div>
              <Applies body={p.body} keyName="permissions.automatic_review" className="push" />
              <button type="button" role="switch" aria-checked={review} aria-label="Automatic review" className="st-switch" onClick={() => p.save.save("permissions.automatic_review", !review)}>
                <span />
              </button>
            </div>
            <div className="st-card-desc">
              Each <span className="mono">bash</span>, <span className="mono">write</span> and <span className="mono">edit</span> call is reviewed before it runs. <span className="mono">fetch</span> keeps its origin grants: allow once, allow origin, or deny.
            </div>
            {reviewError && <FieldError message={reviewError} />}
            <div className="st-steps">
              <Step icon={<Gauge size={13} />} title="1 · Instinct">
                Classifies effects, scope and risk. Read-only and low-risk project changes run.
              </Step>
              <Step icon={<Compass size={13} />} title="2 · Metacognition">
                Must approve risky or outside-project changes before they run.
              </Step>
              <Step icon={<User size={13} />} title="3 · Driver">
                Decides on any doubt, timeout or reviewer failure: allow once or deny. Sensitive reads always end here.
              </Step>
            </div>
            <div className="st-onoff">
              <button type="button" className="st-onoff-card" aria-pressed={review} onClick={() => !review && p.save.save("permissions.automatic_review", true)}>
                <b>On</b>
                <span>Instinct, then the metacognition for risky calls, then you when either is unsure.</span>
              </button>
              <button type="button" className="st-onoff-card" aria-pressed={!review} onClick={() => review && p.save.save("permissions.automatic_review", false)}>
                <b>Off</b>
                <span>Every bash, write and edit call is asked to you.</span>
              </button>
            </div>
          </div>
        )}
        {p.visible("perm:scratch") && (
          <div className="st-card">
            <div className="st-card-top left">
              <div className="st-card-title">Scratch directories</div>
              <span className="mono st-key">permissions.scratch_dirs</span>
              <Applies body={p.body} keyName="permissions.scratch_dirs" className="push" />
            </div>
            <div className="st-card-desc">Agents may write here without asking. Absolute paths only. An empty list turns scratch space off.</div>
            <div className="st-dirs">
              {dirs.map((d) => (
                <div key={d} className="st-dir">
                  <Folder size={14} aria-hidden />
                  <span className="mono">{d}</span>
                  {!dirsSet && defaults.includes(d) && <span className="muted small">default</span>}
                  <button type="button" className="icon-btn end" aria-label={`Remove ${d}`} title="Remove" onClick={() => p.save.save("permissions.scratch_dirs", dirs.filter((x) => x !== d))}>
                    <X size={13} />
                  </button>
                </div>
              ))}
              {!dirs.length && <div className="st-dir muted">No scratch directory: scratch space is off.</div>}
            </div>
            <form
              className="st-dir-add"
              onSubmit={(e) => {
                e.preventDefault();
                add();
              }}
            >
              <input
                className="st-input mono"
                value={draft}
                onChange={(e) => {
                  setDraft(e.target.value);
                  setLocal(null);
                }}
                placeholder="/absolute/path"
                aria-label="Directory to add"
              />
              <button type="submit" className="btn" disabled={!draft.trim()}>
                Add
              </button>
              <button type="button" className="btn ghost" disabled={!dirsSet} title={dirsSet ? "Remove the key from config.json" : "Already the default"} onClick={() => p.save.save("permissions.scratch_dirs", null)}>
                Reset to default
              </button>
            </form>
            {(local ?? dirsError) && <FieldError message={(local ?? dirsError)!} />}
          </div>
        )}
        {!p.visible("perm:review") && !p.visible("perm:scratch") && <div className="st-empty">No permission setting matches this search.</div>}
      </div>
    </>
  );
}

function Step({ icon, title, children }: { icon: React.ReactNode; title: string; children: React.ReactNode }) {
  return (
    <div className="st-step">
      <div className="t">
        <span className="i">{icon}</span>
        {title}
      </div>
      <div className="d">{children}</div>
    </div>
  );
}

// ---------- Subagents ----------

const MAX = 64;

export function Subagents(p: SectionProps) {
  const maxKey = "subagents.max_concurrent";
  const max = asInt(shown(p, maxKey), 4);
  const maxError = problem(p, maxKey);
  const setMax = (n: number) => {
    const next = Math.min(MAX, Math.max(1, n));
    if (next !== max) p.save.save(maxKey, next);
  };
  const watchdogs = [
    { id: "sub:watchdog", key: "subagents.watchdog_interval_secs", title: "Subagent watchdog", desc: "How often a running child is checked for progress. Off disables child supervision." },
    { id: "sub:root", key: "agent.watchdog_interval_secs", title: "Root agent watchdog", desc: "The same progress check for the session's own agent." },
  ];
  const any = p.visible("sub:max") || watchdogs.some((w) => p.visible(w.id));
  return (
    <>
      <Head title="Subagents" file="config.json → subagents, agent">
        Background children and the watchdogs that check on them.
      </Head>
      <div className="st-stack">
        {p.visible("sub:max") && (
          <div className="st-card">
            <div className="st-card-top left">
              <div className="st-card-title">Max concurrent per session</div>
              <span className="mono st-key">{maxKey}</span>
              <Applies body={p.body} keyName={maxKey} className="push" />
            </div>
            <div className="st-card-desc">Children starting, running or waiting for an answer all count. 1 to 64, default 4.</div>
            <div className="st-row">
              <div className="st-stepper" role="group" aria-label="Max concurrent subagents">
                <button type="button" aria-label="One less" disabled={max <= 1} onClick={() => setMax(max - 1)}>
                  −
                </button>
                <input
                  className="mono"
                  inputMode="numeric"
                  aria-label="Max concurrent subagents"
                  key={max}
                  defaultValue={max}
                  onBlur={(e) => {
                    const n = Number.parseInt(e.target.value, 10);
                    if (Number.isFinite(n)) setMax(n);
                    else e.target.value = String(max);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") e.currentTarget.blur();
                    if (e.key === "ArrowUp") (e.preventDefault(), setMax(max + 1));
                    if (e.key === "ArrowDown") (e.preventDefault(), setMax(max - 1));
                  }}
                />
                <button type="button" aria-label="One more" disabled={max >= MAX} onClick={() => setMax(max + 1)}>
                  +
                </button>
              </div>
              <div className="st-slots" aria-hidden>
                {Array.from({ length: Math.min(max, 16) }, (_, i) => (
                  <span key={i} />
                ))}
              </div>
              <span className="muted small">
                {max} slot{max === 1 ? "" : "s"} per session{max > 16 ? " (first 16 shown)" : ""}
              </span>
            </div>
            {maxError && <FieldError message={maxError} />}
          </div>
        )}
        {watchdogs.map((w) => {
          if (!p.visible(w.id)) return null;
          const secs = asInt(shown(p, w.key), 300);
          const err = problem(p, w.key);
          const preset = WATCHDOG_PRESETS.some((x) => x.secs === secs);
          return (
            <div key={w.key} className="st-card">
              <div className="st-card-top left">
                <div className="st-card-title">{w.title}</div>
                <span className="mono st-key">{w.key}</span>
                <Applies body={p.body} keyName={w.key} className="push" />
              </div>
              <div className="st-card-desc">{w.desc}</div>
              <div className="st-pills">
                {WATCHDOG_PRESETS.map((x) => (
                  <Pill key={x.secs} on={secs === x.secs} onClick={() => secs !== x.secs && p.save.save(w.key, x.secs)}>
                    {x.label}
                  </Pill>
                ))}
                <span className="mono muted small st-val">
                  {secs} s{preset ? "" : " · custom value"}
                </span>
              </div>
              {err && <FieldError message={err} />}
            </div>
          );
        })}
        {!any && <div className="st-empty">No subagent setting matches this search.</div>}
      </div>
    </>
  );
}

// ---------- Appearance ----------

export function Appearance(p: SectionProps) {
  const saved = asTheme(shown(p, "tui.theme"));
  const err = problem(p, "tui.theme");
  const cards = THEMES.filter((t) => p.visible(`theme:${t.id}`));
  return (
    <>
      <Head title="Appearance" file="config.json → tui.theme">
        Shared by the terminal client and the WebUI.
      </Head>
      <div className="st-applies-line">
        <Applies body={p.body} keyName="tui.theme" />
      </div>
      <div className="st-themes">
        {cards.map((t) => (
          <button
            key={t.id}
            type="button"
            className="st-theme"
            aria-pressed={(saved ?? p.theme) === t.id}
            onClick={() => {
              p.setTheme(t.id);
              if (saved !== t.id) p.save.save("tui.theme", t.id);
            }}
          >
            <span className="st-preview" data-theme={t.id}>
              <span className="side" />
              <span className="lines">
                <span className="l1" />
                <span className="l2" />
                <span className="quote">
                  <span className="l3" />
                  <span className="l4" />
                </span>
                <span className="dots">
                  <span className="gr" />
                  <span className="rd" />
                  <span className="bl" />
                  <span className="pu" />
                </span>
              </span>
            </span>
            <span className="foot">
              <span>{t.label}</span>
              <span className="mono muted small">{t.id}</span>
            </span>
          </button>
        ))}
        {!cards.length && <div className="st-empty">No theme matches this search.</div>}
      </div>
      {!saved && <div className="st-note">config.json has no theme yet: this browser uses {THEMES.find((t) => t.id === p.theme)?.label}. Choosing one saves it for every client.</div>}
      {err && <FieldError message={err} />}
    </>
  );
}
