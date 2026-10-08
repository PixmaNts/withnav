/** The "WithNav Settings" page shell; `out/settings.js` renders everything inside it. */
export function settingsHtml(nonce: string, script: string, style: string, cspSource: string): string {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${cspSource} data:; style-src ${cspSource}; script-src 'nonce-${nonce}'; connect-src 'none'; font-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'">
  <link rel="stylesheet" href="${style}">
  <title>WithNav Settings</title>
</head>
<body>
  <div class="page">
    <header class="header">
      <div class="search-row">
        <div class="search-box">
          <input id="search" type="text" spellcheck="false" autocomplete="off" placeholder="Search WithNav settings" aria-label="Search WithNav settings" aria-describedby="count">
          <span id="count" class="count" aria-live="polite"></span>
        </div>
        <button id="filter" class="icon-button" type="button" title="Show modified settings only" aria-label="Show modified settings only" aria-pressed="false"></button>
        <button id="open-json" class="icon-button" type="button" title="Open config.json" aria-label="Open config.json"></button>
      </div>
      <div class="scope-row">
        <span class="scope-tab" role="tab" aria-selected="true">User</span>
        <span id="save-state" class="save-state" role="status" aria-live="polite"></span>
        <span id="paths" class="paths"></span>
      </div>
    </header>
    <div class="body">
      <nav class="toc" aria-label="Table of contents">
        <div id="toc"></div>
        <p class="toc-footer">Editor integration (idle delay, reflex, server path) lives in <a href="#" id="extension-settings">VS Code settings › withnav.*</a></p>
      </nav>
      <main id="rows" class="rows" tabindex="-1"><p class="placeholder">Loading WithNav settings…</p></main>
    </div>
  </div>
  <script nonce="${nonce}" src="${script}"></script>
</body>
</html>`;
}
