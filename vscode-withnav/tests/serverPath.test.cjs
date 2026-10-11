const { test } = require('node:test');
const assert = require('node:assert/strict');
const { pickServerCommand } = require('../out/tests/serverPath.js');

const base = { configured: 'nav', explicit: false, bundled: '/ext/bin/nav', bundledExists: false };

test('an explicit serverPath wins over the bundled binary', () => {
  assert.deepEqual(
    pickServerCommand({ ...base, configured: '/custom/nav', explicit: true, bundledExists: true }),
    { command: '/custom/nav', source: 'setting' },
  );
  // Setting it to plain "nav" is a deliberate request for PATH.
  assert.deepEqual(
    pickServerCommand({ ...base, explicit: true, bundledExists: true }),
    { command: 'nav', source: 'setting' },
  );
});

test('the bundled binary is used when the user set nothing', () => {
  assert.deepEqual(
    pickServerCommand({ ...base, bundledExists: true }),
    { command: '/ext/bin/nav', source: 'bundled' },
  );
});

test('a universal package falls back to nav on PATH', () => {
  assert.deepEqual(pickServerCommand(base), { command: 'nav', source: 'path' });
});
