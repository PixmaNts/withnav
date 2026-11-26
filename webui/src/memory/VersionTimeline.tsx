import { ArrowCounterClockwise } from "@phosphor-icons/react";
import { useState } from "react";
import type { MemoryGroup, MemoryProject, MemoryVersion } from "../api/types";
import { ago, authorLine, diffText, exactTime, scopeLabel, shortId, type DiffPart } from "./model";

interface Props {
  /** Oldest first, as the register returns them. */
  versions: MemoryVersion[];
  currentVersion: number;
  groups: MemoryGroup[];
  projects: MemoryProject[];
  /** Makes an older wording current again as a new version; absent when the item cannot take it. */
  onUse?: (version: number) => void;
}

function Marked({ parts }: { parts: DiffPart[] }) {
  return (
    <>
      {parts.map((p, i) => (p.kind === "del" ? <del key={i}>{p.text}</del> : p.kind === "add" ? <ins key={i}>{p.text}</ins> : <span key={i}>{p.text}</span>))}
    </>
  );
}

/** The versions of an item, newest first; two picked versions are compared below them. */
export function VersionTimeline({ versions, currentVersion, groups, projects, onUse }: Props) {
  const [picked, setPicked] = useState<number[]>([]);
  const toggle = (n: number) => setPicked((held) => (held.includes(n) ? held.filter((x) => x !== n) : [...held, n].slice(-2)));
  const [from, to] = [...picked].sort((a, b) => a - b).map((n) => versions.find((v) => v.version === n));
  const label = (v: MemoryVersion) => scopeLabel(v.scope, v.scope_ref, groups, projects);

  return (
    <section className="mm-timeline" aria-label="Versions">
      <div className="mm-label">
        VERSIONS · nothing is overwritten
        <span className="mm-hint">{picked.length === 2 ? "Comparing two versions" : "Tick two versions to compare them"}</span>
      </div>
      <ol className="mm-versions">
        {[...versions].reverse().map((v) => (
          <li key={v.version} className="mm-version" data-current={v.version === currentVersion || undefined}>
            <span className="mm-dot" data-change={v.change} aria-hidden />
            <div className="mm-vbody">
              <div className="mm-vhead">
                <span className="mm-vnum mono">v{v.version}</span>
                <span className="mm-change" data-change={v.change}>{v.change}</span>
                <span className="mm-hint" title={exactTime(v.at)}>{ago(v.at)}</span>
                <label className="mm-pick">
                  <input type="checkbox" aria-label={`Compare version ${v.version}`} checked={picked.includes(v.version)} onChange={() => toggle(v.version)} />
                </label>
              </div>
              <div className="mm-hint">
                by {authorLine(v)}
                {v.source_session_id && <> · session <span className="mono">{shortId(v.source_session_id)}</span></>}
                {" · "}{label(v)}
              </div>
              {v.reason && <div className="mm-reason">“{v.reason}”</div>}
              {onUse && v.version !== currentVersion && (
                <button type="button" className="btn ghost mm-small" onClick={() => onUse(v.version)}>
                  <ArrowCounterClockwise size={12} aria-hidden /> Use this wording
                </button>
              )}
            </div>
          </li>
        ))}
      </ol>
      {from && to && (
        <div className="mm-diff" role="group" aria-label={`Changes from version ${from.version} to version ${to.version}`}>
          <div className="mm-label">
            <span className="mono">v{from.version} → v{to.version}</span>
          </div>
          {from.title !== to.title && (
            <p>
              <b>Title</b> <Marked parts={diffText(from.title, to.title)} />
            </p>
          )}
          <p>
            <b>Decision</b> <Marked parts={diffText(from.body, to.body)} />
          </p>
          {(from.why ?? "") !== (to.why ?? "") && (
            <p>
              <b>Why</b> <Marked parts={diffText(from.why ?? "", to.why ?? "")} />
            </p>
          )}
          {(from.scope !== to.scope || from.scope_ref !== to.scope_ref) && (
            <p>
              <b>Scope</b> {from.scope} → {to.scope}
              <span className="mm-hint"> ({label(from)} → {label(to)})</span>
            </p>
          )}
          {to.reason && <p className="mm-hint">Reason for v{to.version}: {to.reason}</p>}
        </div>
      )}
    </section>
  );
}
