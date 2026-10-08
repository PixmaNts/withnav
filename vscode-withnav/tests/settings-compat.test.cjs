const { test } = require('node:test');
const assert = require('node:assert/strict');
const Module = require('node:module');

test('WithNav settings prefer explicit new values and notify once for legacy fallback', () => {
  const values = { withnav: {}, navai: { enabled: false, idleMs: 42 } };
  const notices = [];
  const original = Module._load;
  Module._load = function(name, ...args) {
    if (name === 'vscode') return {
      workspace: { getConfiguration: section => ({
        inspect: key => ({ globalValue: values[section][key] }),
        get: (key, fallback) => values[section][key] ?? fallback,
      }) },
      window: { showInformationMessage: message => notices.push(message) },
    };
    return original.call(this, name, ...args);
  };
  try {
    const { readSetting, affectsSettings } = require('../out/tests/settings.js');
    assert.equal(readSetting('enabled', true), false);
    assert.equal(readSetting('idleMs', 3000), 42);
    assert.equal(notices.length, 1);
    assert.match(notices[0], /navai\.\*.*withnav\.\*/);
    values.withnav.enabled = true;
    assert.equal(readSetting('enabled', false), true);
    assert.equal(readSetting('cooldownMs', 20000), 20000);
    assert.equal(affectsSettings({ affectsConfiguration: key => key === 'navai' }), true);
  } finally { Module._load = original; }
});
