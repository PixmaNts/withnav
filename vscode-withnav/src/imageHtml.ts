// The HTML of the image panel: one image data URL, no script, no networking.

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

export function imageHtml(name: string, url: string): string {
  // Only an image data URL may be shown; anything else renders an empty page.
  const src = /^data:image\/(png|jpeg|gif|webp);base64,[A-Za-z0-9+/=]+$/.test(url) ? url : "";
  return `<!DOCTYPE html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
<style>body{margin:0;display:grid;place-items:center;min-height:100vh;background:var(--vscode-editor-background)}img{max-width:100%;max-height:100vh;object-fit:contain}</style>
</head><body><img src="${src}" alt="${escapeHtml(name)}"></body></html>`;
}
