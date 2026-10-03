import * as vscode from "vscode";

const SECTION = "navai";
/** The section the settings had before the extension was renamed. */
const LEGACY_SECTION = "navigator";

/** True when the user set the key themselves, in any scope. */
function isExplicit(inspected: ReturnType<vscode.WorkspaceConfiguration["inspect"]>): boolean {
  return (
    inspected !== undefined &&
    (inspected.globalValue !== undefined ||
      inspected.workspaceValue !== undefined ||
      inspected.workspaceFolderValue !== undefined)
  );
}

/**
 * Reads the setting `key` (without its section). A value the user set under
 * `navai.` wins; otherwise one set under the former `navigator.` section is
 * honored; otherwise the default.
 */
export function readSetting<T>(key: string, fallback: T): T {
  const current = vscode.workspace.getConfiguration(SECTION);
  if (isExplicit(current.inspect<T>(key))) {
    return current.get<T>(key, fallback);
  }
  const legacy = vscode.workspace.getConfiguration(LEGACY_SECTION).inspect<T>(key);
  if (isExplicit(legacy)) {
    return (legacy?.workspaceFolderValue ?? legacy?.workspaceValue ?? legacy?.globalValue) as T;
  }
  return current.get<T>(key, fallback);
}

/** True when a configuration change touches a Navai setting, old or new. */
export function affectsSettings(event: vscode.ConfigurationChangeEvent): boolean {
  return event.affectsConfiguration(SECTION) || event.affectsConfiguration(LEGACY_SECTION);
}
