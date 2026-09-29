import * as vscode from "vscode";
import { LanguageClient, State } from "vscode-languageclient/node";
import { InlineCompletionParams, InlineCompletionResult, ReflexShownNotification } from "./protocol";

const DEBOUNCE_MS = 150;

/** Reads the reflex on/off setting (declared by the integration step). */
const reflexEnabled = () =>
  vscode.workspace.getConfiguration("navigator").get<boolean>("reflex.enabled", true);

/**
 * The reflex layer's editor half: an inline-completion provider that asks
 * the language server (`navigator/inlineCompletion`, after a 150 ms
 * debounce) and reports display time back (`navigator/reflexShown`) so the
 * server's log holds both halves of the pause-to-display latency.
 *
 * Suggestions apply on Tab (VS Code's inlineSuggest default); continuing
 * to type dismisses them. Returning an empty list hides the ghost text.
 */
export function registerReflex(client: LanguageClient): vscode.Disposable[] {
  const timers = new Map<string, ReturnType<typeof setTimeout>>();

  const provider: vscode.InlineCompletionItemProvider = {
    provideInlineCompletionItems: async (document, position, _token, _ctx) => {
      if (!reflexEnabled()) {
        return { items: [] };
      }
      const uri = document.uri.toString();

      // Debounce: the last keystroke wins; a superseded timer is dropped.
      const pending = timers.get(uri);
      if (pending) {
        clearTimeout(pending);
      }
      await new Promise<boolean>((resolve) => {
        timers.set(uri, setTimeout(() => resolve(true), DEBOUNCE_MS));
      });
      if (timers.get(uri)) {
        timers.delete(uri);
      }
      if (!reflexEnabled()) {
        return { items: [] };
      }

      if (client.state !== State.Running) {
        return { items: [] };
      }
      const params: InlineCompletionParams = {
        uri,
        version: document.version,
        line: position.line,
        character: position.character,
      };
      let result: InlineCompletionResult | undefined;
      try {
        result = await client.sendRequest<InlineCompletionResult>(
          "navigator/inlineCompletion",
          params,
        );
      } catch {
        // A transport failure never breaks the editor: no suggestion (the
        // server logs its own half of the error).
        return { items: [] };
      }
      if (!result || !result.items || result.items.length === 0) {
        return { items: [] };
      }

      // The request id rides on the result; the server's log joins this
      // reflexShown record with its own call record by id.
      const requestId = (result as { requestId?: string }).requestId ?? "";
      if (requestId) {
        void client
          .sendNotification(ReflexShownNotification.type, {
            requestId,
            shownAtMs: Date.now(),
          })
          .catch(() => undefined);
      }

      return {
        items: result.items.map((item) => {
          const range = new vscode.Range(
            new vscode.Position(item.range.start.line, item.range.start.character),
            new vscode.Position(item.range.end.line, item.range.end.character),
          );
          const completion = new vscode.InlineCompletionItem(item.text, range);
          // Kept for the handleInlineCompletionItemAccepted hook.
          (completion as unknown as { requestId?: string }).requestId = requestId;
          return completion;
        }),
      };
    },
  };

  return [vscode.languages.registerInlineCompletionItemProvider({ scheme: "file" }, provider)];
}
