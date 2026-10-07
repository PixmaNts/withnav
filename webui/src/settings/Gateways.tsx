import { Key, LockSimple } from "@phosphor-icons/react";
import { useMutation } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api/client";
import type { GatewayInfo, GatewayTest } from "../api/types";
import { shortPath } from "../lib/format";
import { choiceOf, usedByLabel } from "../lib/settings";
import { Catalogue } from "./Catalogue";
import type { SectionProps } from "./context";
import { Field, Head } from "./ui";

const KEY_SOURCE: Record<GatewayInfo["key_source"], string> = {
  environment: "found in environment",
  auth_store: "found in auth store",
  missing: "no key found",
};

const ms = (n: number | null) => (n === null ? "—" : n.toLocaleString("en-US"));

export function Gateways(p: SectionProps) {
  const gateways = p.body.gateways.filter((g) => p.visible(`gateway:${g.name}`));
  const [chosen, setChosen] = useState<string | null>(null);
  // The design opens on the busiest gateway.
  const busiest = [...gateways].sort((a, b) => b.used_by.length - a.used_by.length)[0];
  const current = gateways.find((g) => g.name === chosen) ?? busiest;
  // One test result per gateway, kept while the page is open.
  const [tests, setTests] = useState<Record<string, GatewayTest | { error: string }>>({});
  const test = useMutation({
    mutationFn: (name: string) => api.testGateway(p.workspace, name),
    onSuccess: (result, name) => setTests((t) => ({ ...t, [name]: result })),
    onError: (error, name) => setTests((t) => ({ ...t, [name]: { error: error instanceof Error ? error.message : String(error) } })),
  });
  useEffect(() => {
    if (chosen && !gateways.some((g) => g.name === chosen)) setChosen(null);
  }, [chosen, gateways]);
  // Each gateway is tested once when the section opens, so the dots say something true;
  // "Test connection" asks again.
  const tested = useRef(new Set<string>());
  useEffect(() => {
    for (const g of p.body.gateways) {
      if (tested.current.has(g.name)) continue;
      tested.current.add(g.name);
      api
        .testGateway(p.workspace, g.name)
        .then((result) => setTests((t) => ({ ...t, [g.name]: result })))
        .catch((error) => setTests((t) => ({ ...t, [g.name]: { error: error instanceof Error ? error.message : String(error) } })));
    }
  }, [p.body.gateways, p.workspace]);

  return (
    <>
      <Head title="Gateways" file="models.json → providers · catalogue only">
        Endpoints that serve the catalogue. Models are named <span className="mono">&lt;gateway&gt;/&lt;id&gt;</span>.
      </Head>
      {!p.body.gateways.length && <div className="st-empty">models.json lists no gateway.</div>}
      {p.body.gateways.length > 0 && !gateways.length && <div className="st-empty">No gateway matches this search.</div>}
      {current && (
        <div className="st-gw">
          <div className="st-gw-list">
            {gateways.map((g) => {
              const t = tests[g.name];
              const dot = !t ? "unknown" : "ok" in t && t.ok ? "ok" : "bad";
              const why = !t ? "Testing…" : "ok" in t ? (t.ok ? `Reachable · ${t.latency_ms} ms` : `Unreachable${t.error ? ` · ${t.error}` : ""}`) : t.error;
              return (
                <button key={g.name} type="button" className="st-gw-card" aria-pressed={g.name === current.name} onClick={() => setChosen(g.name)}>
                  <span className="row">
                    <span className={`dot ${dot}`} title={why} />
                    <span className="mono name">{g.name}</span>
                    <span className="used">used by {g.used_by.length}</span>
                  </span>
                  <span className="mono base">{g.base_url}</span>
                </button>
              );
            })}
            <div className="st-gw-note">
              <LockSimple size={12} aria-hidden /> Gateways are read-only here. Add or edit them in{" "}
              <span className="mono">{p.body.paths.models ? shortPath(p.body.paths.models) : "models.json"}</span>.
            </div>
          </div>
          <GatewayDetail p={p} g={current} test={tests[current.name]} testing={(test.isPending && test.variables === current.name) || !tests[current.name]} onTest={() => test.mutate(current.name)} />
        </div>
      )}
    </>
  );
}

function GatewayDetail({ p, g, test, testing, onTest }: { p: SectionProps; g: GatewayInfo; test: GatewayTest | { error: string } | undefined; testing: boolean; onTest: () => void }) {
  const models = useMemo(() => p.models.filter((m) => m.gateway === g.name), [p.models, g.name]);
  const result = (() => {
    if (testing) return <span className="st-pill mu">testing…</span>;
    if (!test) return null;
    if (!("ok" in test)) return <span className="st-pill bad" title={test.error}>test failed · {test.error}</span>;
    if (test.ok) return <span className="st-pill ok">reachable · {test.latency_ms} ms{test.model_count !== null ? ` · ${test.model_count} models listed` : ""}</span>;
    return (
      <span className="st-pill bad" title={test.error ?? undefined}>
        unreachable{test.status ? ` · HTTP ${test.status}` : ""}
        {test.error && !test.status ? ` · ${test.error}` : ""}
      </span>
    );
  })();
  return (
    <div className="st-box">
      <div className="st-box-head">
        <span className="mono title">{g.name}</span>
        {result}
        <span className="right">
          <button type="button" className="btn" onClick={onTest} disabled={testing}>
            Test connection
          </button>
        </span>
      </div>
      <div className="st-grid">
        <Field label="Base URL" wide>
          <div className="st-input mono">{g.base_url}</div>
        </Field>
        <Field
          label="API key"
          note={
            g.key_source === "missing"
              ? `${g.api_key_env ? `${g.api_key_env} is not set in the environment and the` : "The"} auth store has no key for ${g.name}.`
              : "Read from the environment or the auth store. Never shown or logged."
          }
        >
          <div className="st-input">
            <span className="mono">{g.api_key_env ?? <span className="muted">no variable set</span>}</span>
            <span className={`end ${g.key_source === "missing" ? "bad" : "ok"}`}>
              <Key size={12} aria-hidden /> {KEY_SOURCE[g.key_source]}
            </span>
          </div>
        </Field>
        <Field label="Catalog format" note="How the model list is read from the provider.">
          <div className="st-input">
            <span className="mono">{g.catalog_format ?? "none"}</span>
          </div>
        </Field>
        <Field label="Connect timeout">
          <div className="st-input">
            <span className="mono">{ms(g.connect_timeout_ms)}</span>
            <span className="end">ms</span>
          </div>
        </Field>
        <Field label="Request timeout" note="The instinct always uses its own 2 s deadline.">
          <div className="st-input">
            <span className="mono">{ms(g.request_timeout_ms)}</span>
            <span className="end">ms</span>
          </div>
        </Field>
        <Field label="Headers" wide>
          <div className="st-table">
            <div className="st-table-head">
              <span>Name</span>
              <span>Value</span>
            </div>
            {g.headers.length ? (
              g.headers.map((h) => (
                <div key={h} className="st-table-row">
                  <span className="mono">{h}</span>
                  <span className="muted">not shown (may be a secret)</span>
                </div>
              ))
            ) : (
              <div className="st-table-row muted">No extra headers</div>
            )}
          </div>
        </Field>
      </div>
      <div className="st-box-part">
        <Catalogue models={models} oneGateway showApi loading={p.modelsLoading} label={p.modelsLoading ? "Catalogue · grouped by provider" : `Catalogue · grouped by provider · ${models.length} model${models.length === 1 ? "" : "s"}`} />
      </div>
      <div className="st-box-part">
        <div className="st-label">Used by</div>
        <div className="st-chips">
          {g.used_by.length ? (
            g.used_by.map((key) => (
              <span key={key} className="st-chip">
                <span className="mono">{usedByLabel(key)}</span> <span className="muted">· {choiceOf(p.body, key)?.model ?? key}</span>
              </span>
            ))
          ) : (
            <span className="muted">No layer or agent in config.json uses this gateway.</span>
          )}
        </div>
      </div>
    </div>
  );
}
