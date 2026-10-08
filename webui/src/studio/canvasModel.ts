// Definition (+ optional run) → React Flow nodes and edges, and React Flow changes → definition.
// The definition stays the single source of truth; nothing here keeps state.

import type { Edge, Node, NodeChange } from "@xyflow/react";
import { CARD, cardHeight, completeLayout, EVENT_PORT, portsOf, TRIGGER_ID, wireId, wiresOf } from "./graph";
import type { Definition, FlowNode, RunNode, RunRecord } from "./types";

export interface CardData extends Record<string, unknown> {
  /** null for the trigger card. */
  node: FlowNode | null;
  ports: string[];
  /** A run is shown on the canvas. */
  runMode: boolean;
  reached: boolean;
  run: RunNode | null;
  /** Diagnostics count pointing at this node. */
  errors: number;
}

export interface WireData extends Record<string, unknown> {
  taken: boolean;
  runMode: boolean;
}

export type CardNode = Node<CardData>;
export type WireEdge = Edge<WireData>;

export function cardWidth(node: FlowNode | null): number {
  return node === null ? CARD.triggerWidth : node.type === "action" ? CARD.actionWidth : CARD.width;
}

export function canvasNodes(def: Definition, run: RunRecord | null, errorsByNode: Record<string, number> = {}): CardNode[] {
  const layout = completeLayout(def);
  const runById = new Map((run?.nodes ?? []).map((n) => [n.id, n]));
  const trigger: CardNode = {
    id: TRIGGER_ID,
    type: "trigger",
    position: layout[TRIGGER_ID]!,
    deletable: false,
    width: cardWidth(null),
    height: cardHeight(1),
    data: { node: null, ports: [EVENT_PORT], runMode: !!run, reached: !!run && run.status !== "skipped", run: null, errors: errorsByNode[TRIGGER_ID] ?? 0 },
  };
  const cards = def.node.map((node): CardNode => {
    const r = runById.get(node.id) ?? null;
    const ports = portsOf(node);
    return {
      id: node.id,
      type: node.type === "action" ? "action" : node.type === "expression" ? "expression" : "decision",
      position: layout[node.id]!,
      width: cardWidth(node),
      height: cardHeight(ports.length),
      data: { node, ports, runMode: !!run, reached: r?.status === "completed" || r?.status === "failed", run: r, errors: errorsByNode[node.id] ?? 0 },
    };
  });
  return [trigger, ...cards];
}

export function canvasEdges(def: Definition, run: RunRecord | null): WireEdge[] {
  const runById = new Map((run?.nodes ?? []).map((n) => [n.id, n]));
  const reachedTrigger = !!run && run.status !== "skipped";
  return wiresOf(def).map((w) => {
    const from = runById.get(w.source);
    const taken = !!run && (w.source === TRIGGER_ID ? reachedTrigger : from?.status === "completed" && from.port === w.port);
    return {
      id: wireId(w),
      source: w.source,
      sourceHandle: w.port,
      target: w.target,
      targetHandle: null,
      className: run ? (taken ? "sf-wire taken" : "sf-wire idle") : "sf-wire",
      zIndex: taken ? 1 : 0,
      data: { taken, runMode: !!run },
    };
  });
}

/** Apply drag moves to `layout`; every other kind of change leaves the definition as it is. */
export function definitionFromDrag(def: Definition, changes: NodeChange[]): Definition {
  let layout: Definition["layout"] | null = null;
  for (const c of changes) {
    if (c.type !== "position" || !c.position) continue;
    layout ??= { ...def.layout };
    layout[c.id] = { x: Math.round(c.position.x), y: Math.round(c.position.y) };
  }
  return layout ? { ...def, layout } : def;
}
