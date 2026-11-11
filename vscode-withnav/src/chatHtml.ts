export function chatHtml(nonce: string, script: string, style: string, cspSource: string): string {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${cspSource} data:; style-src ${cspSource}; script-src 'nonce-${nonce}'; connect-src 'none'; font-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'">
  <link rel="stylesheet" href="${style}">
  <title>WithNav</title>
</head>
<body>
  <div class="shell">
    <nav id="tabbar" class="tabbar" aria-label="Conversations"></nav>
    <div id="views" class="views"></div>
    <section id="sessions" class="sessions" aria-label="Sessions" hidden></section>
  </div>
  <script nonce="${nonce}" src="${script}"></script>
</body>
</html>`;
}
