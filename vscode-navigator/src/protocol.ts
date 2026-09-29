import { NotificationType } from "vscode-languageclient";

export interface NavigatorConfig {
  enabled: boolean;
  idleMs: number;
  minChangedLines: number;
  cooldownMs: number;
  modelAlias: string;
}

export interface RemarkParams {
  id: string;
  uri: string;
  line: number;
  text: string;
}

export interface ClearParams {
  uri: string;
}

export type NavigatorState = "idle" | "thinking" | "error";

export interface StatusParams {
  state: NavigatorState;
  message?: string;
}

export type Verdict = "useful" | "noise" | "dismissed";

export interface FeedbackParams {
  id: string;
  verdict: Verdict;
}

export type DiagnosticSeverityName = "error" | "warning" | "info" | "hint";

export interface DiagnosticItem {
  line: number;
  severity: DiagnosticSeverityName;
  message: string;
  source?: string;
}

export interface DiagnosticsParams {
  uri: string;
  items: DiagnosticItem[];
}

export interface IntentParams {
  text: string;
}

// --- Reflex: inline completion ---

export interface InlineCompletionParams {
  uri: string;
  version: number;
  line: number;
  character: number;
}

export interface Position {
  line: number;
  character: number;
}

export interface Range {
  start: Position;
  end: Position;
}

export interface InlineCompletionItem {
  text: string;
  range: Range;
}

export interface InlineCompletionResult {
  items: InlineCompletionItem[];
}

export interface ReflexShownParams {
  requestId: string;
  shownAtMs: number;
}

export namespace RemarkNotification {
  export const type = new NotificationType<RemarkParams>("navigator/remark");
}

export namespace ClearNotification {
  export const type = new NotificationType<ClearParams>("navigator/clear");
}

export namespace StatusNotification {
  export const type = new NotificationType<StatusParams>("navigator/status");
}

export namespace FeedbackNotification {
  export const type = new NotificationType<FeedbackParams>("navigator/feedback");
}

export namespace DiagnosticsNotification {
  export const type = new NotificationType<DiagnosticsParams>("navigator/diagnostics");
}

export namespace IntentNotification {
  export const type = new NotificationType<IntentParams>("navigator/intent");
}

export namespace ReflexShownNotification {
  export const type = new NotificationType<ReflexShownParams>("navigator/reflexShown");
}
