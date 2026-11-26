const { test } = require('node:test');
const assert = require('node:assert/strict');
const { outcomeText } = require('../out/tests/outcome');
test('command outcomes mirror the other clients without JSON', () => {
  assert.equal(outcomeText('model', true, { applies: 'now' }), null);
  for (const applies of ['after_turn', 'next_turn']) assert.equal(outcomeText('model', true, { applies }), 'accepted, applies when this turn ends');
  assert.equal(outcomeText('model', true, { model: 'a', window: 1000000 }), 'a · window 1M');
  assert.equal(outcomeText('context', true, { estimate_total: 1000, window: 1000000, percent: 1 }), 'about 1k of 1M · 1%');
  assert.equal(outcomeText('help', true, { commands: [{ name: 'stop', description: 'Stop' }] }), '/stop · Stop');
  assert.equal(outcomeText('variant', true, { variant: 'high', offered: ['high'] }), 'variant high · on offer: high');
  assert.equal(outcomeText('agent', true, { agent: 'build' }), 'agent build');
  assert.equal(outcomeText('stop', true, {}), 'stopped');
  for (const name of ['new', 'compact', 'unknown']) assert.equal(outcomeText(name, true, {}), null);
  assert.equal(outcomeText('model', false, {}, 'No'), 'No');
});

test('memory and goal-check outcomes become the session transcript lines', () => {
  const item = { id: 'abc123def456', scope: 'project', current_version: 1, current: { title: 'Keep the blue notebook' } };
  assert.equal(
    outcomeText('remember', true, { item, scope_label: 'project WithNav' }),
    'Remembered: "Keep the blue notebook" · project WithNav · undo with /forget abc123def456',
  );
  assert.equal(
    outcomeText('remember', true, { item, notice: 'stored for this session only: project unknown' }),
    'Remembered: "Keep the blue notebook" · project · undo with /forget abc123def456\nstored for this session only: project unknown',
  );
  assert.equal(
    outcomeText('goal_check', true, { note_sent: true, note: 'Criterion 1 is contradicted by cargo test.' }),
    'Goal note · automatic observation, not a Captain message\nCriterion 1 is contradicted by cargo test.',
  );
  // With notes off nothing reached the agent, so the transcript shows nothing either.
  assert.equal(outcomeText('goal_check', true, { note_sent: false, statuses: [] }), null);
});

