export function chatHtml(nonce: string, script: string, style: string, cspSource: string): string {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${cspSource}; style-src ${cspSource}; script-src 'nonce-${nonce}'; connect-src 'none'; font-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'">
  <link rel="stylesheet" href="${style}">
  <title>Navai</title>
</head>
<body>
  <div class="shell">
    <header class="topbar"><span class="identity">Navai</span><span id="status" class="status" role="status" aria-live="polite"><span class="status-dot"></span><span id="status-label">Connecting</span></span></header>
    <main id="transcript" class="transcript" role="log" aria-label="Conversation" aria-live="polite" aria-relevant="additions text"><div id="empty" class="empty">Ask about the code, or describe what you want to do.</div></main>
    <form id="composer" class="composer">
      <label for="prompt" class="sr-only">Message Navai</label>
      <textarea id="prompt" rows="2" placeholder="Message Navai…" disabled></textarea>
      <div class="context-row"><label class="context-toggle"><input type="checkbox" id="include-context" checked><span>Include editor context</span></label><span id="context-preview" class="context-chip" title="Attached to your next message">No editor open</span></div>
      <div class="model-row"><label for="model" class="sr-only">Model for a new conversation</label><select id="model" class="model-select" disabled><option value="">Default model</option></select><span id="usage" class="usage" role="meter" aria-label="Context used" aria-valuemin="0" aria-valuemax="100" hidden><span class="usage-bar"><span id="usage-fill" class="usage-fill"></span></span><span id="usage-label" class="usage-label"></span></span></div>
      <div class="composer-footer"><span class="hint">Enter to send · Shift+Enter for a line</span><div class="actions"><button type="button" id="stop" class="stop" hidden>Stop</button><button type="submit" id="send" disabled>Send <span aria-hidden="true">↗</span></button></div></div>
    </form>
  </div>
  <script nonce="${nonce}" src="${script}"></script>
</body>
</html>`;
}
