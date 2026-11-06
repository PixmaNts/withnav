import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { api } from "../api/client";
import type { ImageRef } from "../api/types";

export interface ShownImage {
  sessionId: string;
  image: ImageRef;
}

interface Scope {
  /** The session whose attachment route serves the images below. */
  sessionId: string | null;
  open: (shown: ShownImage) => void;
}

const ImageScope = createContext<Scope>({ sessionId: null, open: () => {} });

export function ImageProvider({ sessionId, open, children }: { sessionId: string | null; open?: (shown: ShownImage) => void; children: ReactNode }) {
  const parent = useContext(ImageScope);
  return <ImageScope.Provider value={{ sessionId, open: open ?? parent.open }}>{children}</ImageScope.Provider>;
}

// Object URLs of fetched attachments. A session holds at most 64 images; the oldest URLs are
// released past this bound so a long browsing session does not keep every picture in memory.
const MAX_URLS = 128;
const urls = new Map<string, Promise<string>>();

function attachmentUrl(sessionId: string, attachmentId: string): Promise<string> {
  const key = `${sessionId}/${attachmentId}`;
  const known = urls.get(key);
  if (known) return known;
  const loading = api.attachment(sessionId, attachmentId).then((bytes) => URL.createObjectURL(bytes));
  loading.catch(() => urls.delete(key));
  urls.set(key, loading);
  while (urls.size > MAX_URLS) {
    const [oldest, url] = urls.entries().next().value as [string, Promise<string>];
    urls.delete(oldest);
    void url.then((u) => URL.revokeObjectURL(u), () => {});
  }
  return loading;
}

function useAttachmentUrl(sessionId: string | null, attachmentId: string): string | null | "error" {
  const [url, setUrl] = useState<string | null | "error">(null);
  useEffect(() => {
    if (!sessionId) return;
    let live = true;
    attachmentUrl(sessionId, attachmentId).then(
      (u) => live && setUrl(u),
      () => live && setUrl("error"),
    );
    return () => {
      live = false;
    };
  }, [sessionId, attachmentId]);
  return url;
}

export function imageLabel(image: ImageRef): string {
  const size = image.size ? ` · ${humanSize(image.size)}` : "";
  const sides = image.width && image.height ? ` · ${image.width}×${image.height}` : "";
  return `${image.name || "image"}${sides}${size}`;
}

export function humanSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}

/** Thumbnails of stored images; a click opens one large in the session panel. */
export function Thumbnails({ images }: { images: ImageRef[] }) {
  const scope = useContext(ImageScope);
  if (!images.length) return null;
  return (
    <div className="thumbs">
      {images.map((image, i) => (
        <Thumbnail key={`${image.attachment_id}:${i}`} image={image} scope={scope} />
      ))}
    </div>
  );
}

function Thumbnail({ image, scope }: { image: ImageRef; scope: Scope }) {
  const url = useAttachmentUrl(scope.sessionId, image.attachment_id);
  const label = imageLabel(image);
  return (
    <button
      type="button"
      className="thumb"
      title={`${label}: open in the panel`}
      disabled={!scope.sessionId}
      onClick={() => scope.sessionId && scope.open({ sessionId: scope.sessionId, image })}
    >
      {url && url !== "error" ? <img src={url} alt={label} /> : <span className="thumb-missing">{url === "error" ? "unavailable" : "…"}</span>}
    </button>
  );
}

/** One image at full size, for the session panel. */
export function ImageView({ shown }: { shown: ShownImage | null }) {
  const url = useAttachmentUrl(shown?.sessionId ?? null, shown?.image.attachment_id ?? "");
  if (!shown) return <div className="panel-empty">Click an image in the conversation to see it here.</div>;
  const label = imageLabel(shown.image);
  return (
    <>
      <div className="label">IMAGE · {label}</div>
      {url === "error" ? (
        <div className="panel-empty">This image is no longer available from the server.</div>
      ) : url ? (
        <a className="image-view" href={url} target="_blank" rel="noreferrer" title="Open in a new tab">
          <img src={url} alt={label} />
        </a>
      ) : null}
    </>
  );
}
