import * as vscode from "vscode";
import { LanguageClient, State } from "vscode-languageclient/node";
import { readSetting } from "./settings";
import { InlineCompletionParams, InlineCompletionResult, ReflexShownNotification } from "./protocol";

const DEBOUNCE_MS = 150;

/** Reads the reflex on/off setting (declared by the integration step). */
const reflexEnabled = () =>
  readSetting<boolean>("reflex.enabled", true);

/**
 * The reflex layer's editor half: an inline-completion provider that asks
 * the language server (`withnav/inlineCompletion`, after a 150 ms
 * debounce) and reports display time back (`withnav/reflexShown`) so the
 * server's log holds both halves of the pause-to-display latency.
 *
 * Suggestions apply on Tab (VS Code's inlineSuggest default); continuing
 * to type dismisses them. Returning an empty list hides the ghost text.
 */
export function registerReflex(client: LanguageClient): vscode.Disposable[] {
  const timers = new Map<
    string,
    { timer: ReturnType<typeof setTimeout>; resolve: (fire: boolean) => void }
  >();

  const provider: vscode.InlineCompletionItemProvider = {
    provideInlineCompletionItems: async (document, position, _context, token) => {
      if (!reflexEnabled()) {
        return { items: [] };
      }
      const uri = document.uri.toString();

      // Debounce: the last keystroke wins. A superseded request resolves to
      // "do not fire" instead of hanging forever.
      const pending = timers.get(uri);
      if (pending) {
        clearTimeout(pending.timer);
        pending.resolve(false);
      }
      const fire = await new Promise<boolean>((resolve) => {
        const entry = {
          timer: setTimeout(() => {
            timers.delete(uri);
            resolve(true);
          }, DEBOUNCE_MS),
          resolve,
        };
        timers.set(uri, entry);
      });
      if (!fire || token.isCancellationRequested || !reflexEnabled()) {
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
          "withnav/inlineCompletion",
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
          // An inline completion's range is the EXISTING text it replaces.
          // A fill-in-the-middle suggestion inserts at the cursor and replaces
          // nothing, so the range is empty. The server's range spans the
          // inserted text instead, and VS Code silently dropped every
          // suggestion whose range did not start with the typed text
          // (observed 2026-09-29: 118 suggestions sent, none displayed).
          const range = new vscode.Range(position, position);
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
