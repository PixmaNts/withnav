import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

let warned = false;
export function warnLegacy() {
  if (warned) return;
  warned = true;
  console.warn("WithNav: using legacy NAVAI_* or ~/.config/navai configuration; migrate to WITHNAV_* and ~/.config/withnav.");
}
export function devEnv(name: string, env = process.env): string | undefined {
  if (env[`WITHNAV_${name}`] !== undefined) return env[`WITHNAV_${name}`];
  const old = env[`NAVAI_${name}`];
  if (old !== undefined) warnLegacy();
  return old;
}
export function devConfigDir(env = process.env, home = homedir(), exists = existsSync): string {
  const explicit = devEnv("CONFIG_DIR", env);
  if (explicit !== undefined) return explicit;
  const current = join(home, ".config", "withnav");
  if (exists(current)) return current;
  warnLegacy();
  return join(home, ".config", "navai");
}
