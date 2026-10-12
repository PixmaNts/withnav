const { test } = require('node:test');
const assert = require('node:assert/strict');
const { bundledServerName, pickServerCommand } = require('../out/tests/serverPath.js');

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

test('the bundled executable name follows the platform', () => {
  assert.equal(bundledServerName('win32'), 'nav.exe');
  assert.equal(bundledServerName('linux'), 'nav');
  assert.equal(bundledServerName('darwin'), 'nav');
});

test('Windows paths with spaces stay unquoted and preserve resolution precedence', () => {
  const bundled = 'C:\\Program Files\\WithNav\\bin\\nav.exe';
  const configured = 'C:\\My Tools\\nav.exe';
  const input = { configured, explicit: true, bundled, bundledExists: true };
  assert.deepEqual(pickServerCommand(input), { command: configured, source: 'setting' });
  assert.deepEqual(pickServerCommand({ ...input, explicit: false }),
    { command: bundled, source: 'bundled' });
  assert.deepEqual(pickServerCommand({ ...input, configured: 'nav', explicit: false, bundledExists: false }),
    { command: 'nav', source: 'path' });
});
