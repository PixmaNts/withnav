/** Where the language server command came from. */
export type ServerSource = "setting" | "bundled" | "path";

export interface ServerChoice {
  command: string;
  source: ServerSource;
}

/**
 * Picks the server command: a path the user set wins, then the binary shipped inside a
 * platform-specific package, then `nav` from PATH. Kept free of the editor API so the order
 * is testable; a universal package has no bundled binary and so falls through to PATH.
 */
export function pickServerCommand(input: {
  configured: string;
  explicit: boolean;
  bundled: string;
  bundledExists: boolean;
}): ServerChoice {
  if (input.explicit) {
    return { command: input.configured, source: "setting" };
  }
  if (input.bundledExists) {
    return { command: input.bundled, source: "bundled" };
  }
  return { command: input.configured, source: "path" };
}
