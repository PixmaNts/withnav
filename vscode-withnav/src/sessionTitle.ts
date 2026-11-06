// Human titles for conversations. A session the server never titled is listed under its id; the id
// is never shown, the first Captain message (or a plain label) stands in.

const ID_LIKE = /^(?:[0-9A-HJKMNP-TV-Z]{20,32}|[0-9a-f]{8}(?:-?[0-9a-f]{4}){3}-?[0-9a-f]{12})$/i;
const MAX_TITLE = 40;

export const isIdTitle = (title: string, sessionId = ""): boolean => {
  const text = title.trim();
  return !text || text === sessionId || ID_LIKE.test(text);
};

export function snippet(text: string, max = MAX_TITLE): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1).trimEnd()}…` : flat;
}

/** `sessionId` empty means a conversation that has not started yet. */
export function displayTitle(raw: string, sessionId: string, firstCaptain = ""): string {
  if (!isIdTitle(raw, sessionId)) return raw.trim();
  if (firstCaptain.trim()) return snippet(firstCaptain);
  return sessionId || raw.trim() ? "Untitled conversation" : "New conversation";
}
