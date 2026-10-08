import * as vscode from "vscode";

const SECTION = "withnav";
/** The section the settings had before the extension was renamed. */
const LEGACY_SECTION = "navai";
let migrationNoticed = false;

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
 * `withnav.` wins; otherwise one set under the former `navai.` section is
 * honored; otherwise the default.
 */
export function readSetting<T>(key: string, fallback: T): T {
  const current = vscode.workspace.getConfiguration(SECTION);
  if (isExplicit(current.inspect<T>(key))) {
    return current.get<T>(key, fallback);
  }
  const legacy = vscode.workspace.getConfiguration(LEGACY_SECTION).inspect<T>(key);
  if (isExplicit(legacy)) {
    if (!migrationNoticed) {
      migrationNoticed = true;
      void vscode.window.showInformationMessage("WithNav is using navai.* settings. Migrate them to withnav.*; legacy settings remain readable during 0.7.");
    }
    return (legacy?.workspaceFolderValue ?? legacy?.workspaceValue ?? legacy?.globalValue) as T;
  }
  return current.get<T>(key, fallback);
}

/** True when a configuration change touches a WithNav setting, old or new. */
export function affectsSettings(event: vscode.ConfigurationChangeEvent): boolean {
  return event.affectsConfiguration(SECTION) || event.affectsConfiguration(LEGACY_SECTION);
}
