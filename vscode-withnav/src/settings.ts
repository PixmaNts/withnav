import * as vscode from "vscode";

const SECTION = "withnav";
/** The section the settings had before the extension was renamed. */
const LEGACY_SECTION = "navai";
let migrationNoticed = false;
let serverPathMigrationNoticed = false;

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
export function readSetting<T>(key: string, fallback: T, notifyLegacy = true): T {
  const current = vscode.workspace.getConfiguration(SECTION);
  if (isExplicit(current.inspect<T>(key))) {
    return current.get<T>(key, fallback);
  }
  const legacy = vscode.workspace.getConfiguration(LEGACY_SECTION).inspect<T>(key);
  if (isExplicit(legacy)) {
    if (notifyLegacy && !migrationNoticed) {
      migrationNoticed = true;
      void vscode.window.showInformationMessage("WithNav is using navai.* settings. Migrate them to withnav.*; legacy settings remain readable during 0.7.");
    }
    return (legacy?.workspaceFolderValue ?? legacy?.workspaceValue ?? legacy?.globalValue) as T;
  }
  return current.get<T>(key, fallback);
}

/** True when the user set `key` under `withnav.` or the former `navai.` section, in any scope. */
export function isSettingExplicit(key: string): boolean {
  return (
    isExplicit(vscode.workspace.getConfiguration(SECTION).inspect(key)) ||
    isExplicit(vscode.workspace.getConfiguration(LEGACY_SECTION).inspect(key))
  );
}

/** Old editor binaries shared their directory with nav; preserve that install location. */
export function readServerPath(): string {
  const configured = readSetting<string>("serverPath", "nav", false);
  const migrated = configured.replace(/(^|[\\/])(withnav-lsp|navai-lsp)$/, "$1nav");
  if (migrated !== configured && !serverPathMigrationNoticed) {
    serverPathMigrationNoticed = true;
    void vscode.window.showInformationMessage(`The WithNav editor server is now nav lsp. Update withnav.serverPath from ${configured} to ${migrated}.`);
  }
  return migrated;
}

/** True when a configuration change touches a WithNav setting, old or new. */
export function affectsSettings(event: vscode.ConfigurationChangeEvent): boolean {
  return event.affectsConfiguration(SECTION) || event.affectsConfiguration(LEGACY_SECTION);
}
