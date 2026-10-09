const { test } = require('node:test');
const assert = require('node:assert/strict');
const { JSDOM } = require('jsdom');
const fs = require('node:fs');
test('mode indicator carries both states, pending state and no transcript noise', () => {
  const { chatHtml } = require('../out/tests/chatHtml.js');
  const dom = new JSDOM(chatHtml('n', 'chat.js', 'chat.css', 'resource:'), { runScripts: 'outside-only' });
  dom.window.acquireVsCodeApi = () => ({ postMessage() {}, getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync(require.resolve('../out/chat.js'), 'utf8'));
  const send = data => dom.window.dispatchEvent(new dom.window.MessageEvent('message', { data: { ...data, generation: 1 } }));
  send({ type: 'conversationOpened', id: 'S', title: 'Mode' });
  const button = dom.window.document.querySelector('.bypass-toggle');
  for (let i = 0; i < 10; i++) {
    const enabled = i % 2 === 0;
    send({ type: 'bypass', enabled, pending: false, disabled: false });
    assert.equal(button.getAttribute('aria-label'), enabled ? 'Bypass active' : 'Safe mode');
    assert.equal(button.getAttribute('aria-pressed'), String(enabled));
  }
  assert.equal(dom.window.document.querySelectorAll('.control-notice').length, 0);
  assert.equal(dom.window.document.querySelector('.bypass-badge'), null);
  send({ type: 'bypass', enabled: false, pending: true, disabled: false });
  assert.equal(button.disabled, true);
  assert.equal(button.dataset.pending, 'true');
  dom.window.close();
});
