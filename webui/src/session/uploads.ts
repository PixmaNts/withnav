import { api } from "../api/client";

/** Files already stored, by session: the Composer keeps a failed message's files for the retry. */
const uploaded = new WeakMap<File, { session: string; id: string }>();

/** Uploads at once: enough to overlap round trips, few enough not to flood the local server. */
export const UPLOAD_CONCURRENCY = 3;

/**
 * Stores a message's images in the session, at most `limit` at a time, and returns their
 * attachment ids in the order of `files`. A file already stored in this session (a retry after a
 * failed send) is not sent again; the same file picked twice is sent once.
 *
 * After a failure no further upload starts; the ones in flight are awaited so the files they stored
 * are reused by the retry, then the first error is thrown.
 */
export async function uploadImages(
  sessionId: string,
  files: File[],
  upload: (sessionId: string, file: File) => Promise<{ attachment_id: string }> = api.upload,
  limit = UPLOAD_CONCURRENCY,
): Promise<string[]> {
  const ids = new Map<File, string>();
  const todo: File[] = [];
  for (const file of files) {
    const known = uploaded.get(file);
    if (known?.session === sessionId) ids.set(file, known.id);
    else if (!todo.includes(file)) todo.push(file);
  }
  let next = 0;
  const errors: unknown[] = [];
  const worker = async () => {
    while (errors.length === 0 && next < todo.length) {
      const file = todo[next++]!;
      try {
        const id = (await upload(sessionId, file)).attachment_id;
        uploaded.set(file, { session: sessionId, id });
        ids.set(file, id);
      } catch (error) {
        errors.push(error);
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(Math.max(1, limit), todo.length) }, worker));
  if (errors.length) throw errors[0];
  return files.map((file) => ids.get(file)!);
}
