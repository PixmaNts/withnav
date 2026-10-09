const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

test('chat and settings follow the editor font and scale secondary text', () => {
  for (const name of ['chat', 'settings']) {
    const css = fs.readFileSync(path.join(__dirname, '../src/webview', `${name}.css`), 'utf8');
    assert.match(css, /font(?:-size)?: var\(--vscode-editor-font-size\)/);
    assert.doesNotMatch(css, /font-size:\s*[\d.]+px/);
  }
});
