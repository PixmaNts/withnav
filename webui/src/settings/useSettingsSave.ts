import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useReducer, useRef } from "react";
import { api } from "../api/client";
import { HttpError } from "../api/http";
import { keys } from "../api/queries";
import type { SettingsBody } from "../api/types";
import { initialSave, saveReducer, type SaveState } from "../lib/settings";

export interface SettingsSave {
  state: SaveState;
  /**
   * Writes one key now (`null` removes it). Changes go out one at a time, in order. `control`
   * names the control that made the change, so a refusal is shown next to it.
   */
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  save: (key: string, value: any, control?: string) => void;
  /** The value a control shows: the one being saved, else the server's. */
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  pending: (key: string) => { has: boolean; value: any };
}

/**
 * Immediate saves against `PUT /v1/settings`. Each change carries the revision of the body last
 * read; the answer is the new body, so the cache never needs a refetch. A 409 means config.json
 * changed elsewhere: the page reloads it and drops the changes still queued, rather than replaying
 * them over a file the user has not seen. A 400 stays attached to its key.
 */
export function useSettingsSave(workspace: string | null): SettingsSave {
  const qc = useQueryClient();
  const [state, dispatch] = useReducer(saveReducer, initialSave);
  const queue = useRef<Promise<void>>(Promise.resolve());
  const generation = useRef(0);

  const save = useCallback(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (key: string, value: any, control?: string) => {
      if (!workspace) return;
      const queryKey = keys.settings(workspace);
      const mine = generation.current;
      dispatch({ type: "start", key, value, control });
      queue.current = queue.current.then(async () => {
        if (mine !== generation.current) return;
        const current = qc.getQueryData<SettingsBody>(queryKey);
        if (!current) {
          dispatch({ type: "failed", key, message: "settings are not loaded" });
          return;
        }
        try {
          const body = await api.saveSetting(workspace, { revision: current.revision, key, value });
          qc.setQueryData(queryKey, body);
          dispatch({ type: "saved", key });
          // Layer status and agent assignments are projections of config.json.
          void qc.invalidateQueries({ queryKey: keys.layers(workspace) });
          void qc.invalidateQueries({ queryKey: ["agents", workspace] });
        } catch (error) {
          if (error instanceof HttpError && error.status === 409) {
            generation.current += 1;
            dispatch({ type: "stale" });
            await qc.refetchQueries({ queryKey });
          } else if (error instanceof HttpError && error.status === 400) {
            dispatch({ type: "invalid", key, message: error.message });
          } else {
            dispatch({ type: "failed", key, message: error instanceof Error ? error.message : String(error) });
          }
        }
      });
    },
    [qc, workspace],
  );

  const pending = (key: string) => ({ has: key in state.pending, value: state.pending[key] });

  return { state, save, pending };
}
