// The composer's image chooser: a native multi-select dialog whose files the host reads, so
// images can be attached without the clipboard. Only files the Captain picked are read.
import * as vscode from "vscode";
import type { PickedImage } from "./chatProtocol";

/** Most images a message carries, and the largest file read: the server's 3.75 MiB per image. */
const MAX_IMAGES = 8;
const MAX_IMAGE_FILE = 3_932_160;
const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

/**
 * Asks for images and reads at most `room` of them. A file that is empty or too large is named
 * without being read; once `room` images are read the rest are named as omitted. Cancelling
 * returns nothing.
 */
export async function pickImageFiles(room: number): Promise<PickedImage[]> {
  const uris = await vscode.window.showOpenDialog({
    canSelectMany: true,
    canSelectFiles: true,
    canSelectFolders: false,
    openLabel: "Attach",
    title: "Attach images",
    filters: { Images: IMAGE_EXTENSIONS },
  });
  if (!uris?.length) return [];
  const capacity = Math.max(0, Math.min(MAX_IMAGES, Math.floor(room)));
  const picked: PickedImage[] = [];
  let read = 0;
  for (const uri of uris) {
    const name = uri.path.split("/").pop() || "image";
    if (read >= capacity) { picked.push({ name, problem: "omitted" }); continue; }
    try {
      const { size } = await vscode.workspace.fs.stat(uri);
      if (size === 0) { picked.push({ name, problem: "empty" }); continue; }
      if (size > MAX_IMAGE_FILE) { picked.push({ name, problem: "large" }); continue; }
      // The file can change between the two calls: the bytes read are what is checked.
      const bytes = await vscode.workspace.fs.readFile(uri);
      if (!bytes.length) { picked.push({ name, problem: "empty" }); continue; }
      if (bytes.length > MAX_IMAGE_FILE) { picked.push({ name, problem: "large" }); continue; }
      picked.push({ name, size: bytes.length, data: Buffer.from(bytes).toString("base64") });
      read++;
    } catch {
      picked.push({ name, problem: "unreadable" });
    }
  }
  return picked;
}
