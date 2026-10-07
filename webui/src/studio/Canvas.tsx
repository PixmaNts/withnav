// The canvas: React Flow over the definition. Cards are derived from the definition every render;
// React Flow keeps only what it measures. Drags write `layout`, connections write `start`/`next`.

import {
  applyNodeChanges, Background, BackgroundVariant, Handle, Position, ReactFlow, type Connection, type EdgeChange,
  type NodeChange, type NodeProps,
} from "@xyflow/react";
import { WarningCircle } from "@phosphor-icons/react";
import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { canvasEdges, canvasNodes, cardWidth, definitionFromDrag, type CardNode, type WireEdge } from "./canvasModel";
import { CARD, connect, connectionError, disconnect, parseWireId, removeNode, TRIGGER_ID } from "./graph";
import { actionLook, lookOf, portLabel, runText } from "./meta";
import type { Definition, Diagnostic, FlowNode, RunRecord } from "./types";

interface CardExtra {
  labels: string[];
  stateLine: string;
  body: string;
  name: string;
  meta: string;
  failed: boolean;
  takenPort: string | null;
}
type Card = CardNode & { data: CardNode["data"] & CardExtra };

function CardView({ data, selected }: NodeProps<Card>) {
  const look = lookOf(data.node);
  const node = data.node;
  const strip = !data.runMode ? "no run yet" : node ? runText(node, data.run, data.failed) : data.reached ? "event" : "skipped by when";
  const cls = ["sf-card", selected && "selected", data.runMode && (data.reached ? "reached" : "unreached"), data.errors && "has-errors"].filter(Boolean).join(" ");
  return (
    <div className={cls} style={{ width: cardWidth(node) }}>
      {node && <Handle type="target" position={Position.Left} className="sf-in" style={{ top: CARD.header / 2 }} />}
      <div className="sf-card-head" style={{ height: CARD.header }}>
        <span className="sf-card-icon" style={{ color: look.color }}>{look.icon(14)}</span>
        <span className="sf-card-tag" style={{ color: look.color }}>{look.tag}</span>
        {data.errors > 0 && (
          <span className="sf-card-err" title={`${data.errors} error${data.errors > 1 ? "s" : ""}`}>
            <WarningCircle size={12} weight="fill" />
          </span>
        )}
        <span className="sf-card-name mono">{data.name}</span>
      </div>
      <div className="sf-card-body" style={{ height: CARD.body }}>
        <div className="sf-card-text">{data.body || <span className="muted">No instructions yet.</span>}</div>
        <div className="sf-card-state mono">{data.stateLine}</div>
      </div>
      <div className={`sf-card-strip${data.runMode && data.reached ? " on" : ""}`} style={{ height: CARD.strip }}>
        <span className="mono sf-strip-txt">{strip}</span>
        {data.meta && <span className="mono sf-strip-meta">{data.meta}</span>}
      </div>
      {data.ports.map((port, i) => (
        <div key={port} className={`sf-port${data.takenPort === port ? " taken" : ""}`} style={{ height: CARD.port }}>
          <span className="mono">{data.labels[i]}</span>
          {/* Centred on its 22px row, i.e. at portY(i) from the top of the card. */}
          <Handle type="source" id={port} position={Position.Right} className="sf-out" />
        </div>
      ))}
    </div>
  );
}

// Defined once, outside render: React Flow re-mounts every card when this object changes.
const nodeTypes = { trigger: memo(CardView), decision: memo(CardView), expression: memo(CardView), action: memo(CardView) };

function cardBody(node: FlowNode | null, def: Definition): { name: string; body: string; stateLine: string } {
  if (!node) {
    return {
      name: def.flow.on,
      body: def.flow.when ? `when ${def.flow.when}` : "Every event of this kind.",
      stateLine: def.flow.blocking ? `blocking · ${def.flow.budget_ms ?? "?"} ms` : "asynchronous",
    };
  }
  const name = node.label ? `${node.label}` : node.id;
  const inputs = Object.keys(node.inputs);
  if (node.type === "expression") {
    const assigns = Object.entries(node.assign).map(([k, v]) => `${k} = ${v}`);
    return { name, body: node.condition, stateLine: assigns.length ? assigns.join("; ") : inputs.length ? `reads: ${inputs.join(", ")}` : "No instinct call." };
  }
  if (node.type === "action") {
    const first = Object.values(node.params).find((v) => typeof v === "string" && v) as string | undefined;
    return { name, body: first ?? actionLook(node.action).describe, stateLine: `${actionLook(node.action).label} · ${node.action}` };
  }
  return { name, body: node.instructions, stateLine: inputs.length ? `state: ${inputs.join(", ")}` : "state: (empty)" };
}

export interface CanvasProps {
  def: Definition;
  run: RunRecord | null;
  diagnostics: Diagnostic[];
  selected: string;
  onSelect: (id: string) => void;
  edit: ((f: (def: Definition) => Definition) => void) | null;
  onRefused: (message: string) => void;
}

export function Canvas({ def, run, diagnostics, selected, onSelect, edit, onRefused }: CanvasProps) {
  const errorsByNode = useMemo(() => {
    const out: Record<string, number> = {};
    for (const d of diagnostics) {
      const id = d.node_id ?? (d.path.startsWith("flow") ? TRIGGER_ID : null);
      if (id) out[id] = (out[id] ?? 0) + 1;
    }
    return out;
  }, [diagnostics]);

  const derived = useMemo((): Card[] => {
    const failed = run?.status === "failure";
    const batches = new Map((run?.batches ?? []).map((b) => [b.id, b]));
    return canvasNodes(def, run, errorsByNode).map((n) => {
      const node = n.data.node;
      const r = n.data.run;
      const batch = r?.batch_id ? batches.get(r.batch_id) : undefined;
      const shared = batch && batch.node_ids.length > 1 ? " · shared" : "";
      const meta = r?.status === "completed" && r.duration_ms !== null && node && node.type !== "action" ? `${r.duration_ms} ms${batch?.usage ? ` · ${batch.usage.input_tokens + batch.usage.output_tokens} tok${shared}` : ""}` : "";
      return {
        ...n,
        selected: n.id === selected,
        data: {
          ...n.data,
          ...cardBody(node, def),
          labels: n.data.ports.map((p) => portLabel(node, p, def)),
          meta,
          failed,
          takenPort: r?.status === "completed" ? r.port : n.id === TRIGGER_ID && n.data.reached ? "event" : null,
        },
      };
    });
  }, [def, run, errorsByNode, selected]);

  // React Flow's own measurements survive re-derivation; everything else comes from the definition.
  const [nodes, setNodes] = useState<Card[]>(derived);
  useEffect(() => {
    setNodes((prev) => {
      const measured = new Map(prev.map((n) => [n.id, n.measured]));
      return derived.map((n) => ({ ...n, measured: measured.get(n.id) }));
    });
  }, [derived]);

  const [edgeSel, setEdgeSel] = useState<string | null>(null);
  const edges = useMemo((): WireEdge[] => canvasEdges(def, run).map((e) => ({ ...e, selected: e.id === edgeSel })), [def, run, edgeSel]);

  const onNodesChange = useCallback(
    (changes: NodeChange<Card>[]) => {
      setNodes((ns) => applyNodeChanges(changes, ns));
      if (!edit) return;
      if (changes.some((c) => c.type === "position" && c.position)) edit((d) => definitionFromDrag(d, changes as NodeChange[]));
      for (const c of changes) if (c.type === "remove" && c.id !== TRIGGER_ID) edit((d) => removeNode(d, c.id));
    },
    [edit],
  );

  const onEdgesChange = useCallback(
    (changes: EdgeChange<WireEdge>[]) => {
      for (const c of changes) {
        if (c.type === "select") setEdgeSel((cur) => (c.selected ? c.id : cur === c.id ? null : cur));
        if (c.type === "remove" && edit) {
          const w = parseWireId(c.id);
          if (w) edit((d) => disconnect(d, w));
        }
      }
    },
    [edit],
  );

  const asWire = (c: Connection | WireEdge) => ({ source: c.source, port: c.sourceHandle ?? "", target: c.target });
  const isValidConnection = useCallback((c: Connection | WireEdge) => connectionError(def, asWire(c)) === null, [def]);
  const onConnect = useCallback(
    (c: Connection) => {
      const why = connectionError(def, asWire(c));
      if (why) onRefused(why);
      else edit?.((d) => connect(d, asWire(c)));
    },
    [def, edit, onRefused],
  );

  return (
    <div className={`sf-canvas${run ? " run-mode" : ""}`}>
      <ReactFlow<Card, WireEdge>
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onConnect={onConnect}
        isValidConnection={isValidConnection}
        onNodeClick={(_, n) => onSelect(n.id)}
        nodesDraggable={!!edit}
        nodesConnectable={!!edit}
        deleteKeyCode={edit ? ["Delete", "Backspace"] : null}
        fitView
        fitViewOptions={{ padding: 0.12, maxZoom: 1 }}
        minZoom={0.3}
        maxZoom={1.6}
        proOptions={{ hideAttribution: true }}
        defaultEdgeOptions={{ type: "default" }}
      >
        <Background variant={BackgroundVariant.Dots} gap={18} size={1.2} />
      </ReactFlow>
    </div>
  );
}
