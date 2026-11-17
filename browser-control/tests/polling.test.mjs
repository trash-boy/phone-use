import test from 'node:test';
import assert from 'node:assert/strict';
import { startSessionPolling } from '../web/polling.mjs';

test('a stalled screenshot does not block status polling, and late frames are discarded after cleanup', async () => {
  let refreshes = 0, release;
  const frames = [], errors = [], timers = new Map();
  const session = { id: 'first', deviceId: 'phone', ready: true, daemon: { state: { state: 'ready' } } };
  const client = {
    snapshot: () => ({ session }),
    refresh: async () => { refreshes++; return session; },
    screen: () => new Promise(resolve => { release = resolve; }),
  };
  const stop = startSessionPolling(client, {
    screenshots: true, onScreen: value => frames.push(value), onError: value => errors.push(value),
    schedule: (fn, ms) => { const id = Symbol(); timers.set(id, { fn, ms }); return id; },
    unschedule: id => timers.delete(id),
  });
  await new Promise(resolve => setImmediate(resolve));
  for (let i = 0; i < 3; i++) {
    const timer = [...timers.entries()].find(([, timer]) => timer.ms === 1000);
    assert.ok(timer, 'status timer remains available while the screenshot is pending');
    timers.delete(timer[0]); timer[1].fn();
    await new Promise(resolve => setImmediate(resolve));
  }
  assert.equal(refreshes, 4);
  stop();
  release({ deviceId: 'phone', imageBase64: 'late' });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(frames, []); assert.deepEqual(errors, []); assert.equal(timers.size, 0);
});

test('a reconnect to the same phone cannot receive the previous session image', async () => {
  let release;
  let session = { id: 'old', deviceId: 'phone', ready: true, daemon: { state: { state: 'ready' } } };
  const frames = [];
  const stop = startSessionPolling({
    snapshot: () => ({ session }), refresh: async () => session,
    screen: () => new Promise(resolve => { release = resolve; }),
  }, { screenshots: true, onScreen: frame => frames.push(frame), onError: () => {} });
  session = { ...session, id: 'new' };
  release({ deviceId: 'phone', imageBase64: 'old-image' });
  await new Promise(resolve => setImmediate(resolve));
  stop(); assert.deepEqual(frames, []);
});


test('default idle polling refreshes status without opening any screen requests', async () => {
  const timers = new Map(); let screens = 0, refreshes = 0;
  const session = { id: 'ios', deviceId: 'phone', ready: true, daemon: {state: {state: 'ready'}} };
  const stop = startSessionPolling({
    snapshot: () => ({session}), refresh: async () => { refreshes++; return session; },
    screen: async () => { screens++; return {deviceId: 'phone'}; },
  }, {
    onScreen() {}, onError() {},
    schedule: (fn, ms) => { const id = Symbol(); timers.set(id, {fn, ms}); return id; },
    unschedule: id => timers.delete(id),
  });
  for (let i = 0; i < 5; i++) {
    await new Promise(resolve => setImmediate(resolve));
    const [id, timer] = [...timers.entries()][0];
    assert.equal(timer.ms, 1000);
    timers.delete(id); timer.fn();
  }
  await new Promise(resolve => setImmediate(resolve));
  stop();
  assert.equal(screens, 0);
  assert.equal(refreshes, 6);
  assert.equal(timers.size, 0);
});

function harness(overrides = {}) {
  const timers = new Map();
  const session = { id: 'backoff', deviceId: 'phone', ready: true, daemon: { state: { state: 'ready' } } };
  const client = { snapshot: () => ({ session }), refresh: async () => session, screen: async () => ({ deviceId: 'phone' }), ...overrides.client };
  const options = {
    onScreen() {}, onError() {},
    schedule: (fn, ms) => { const id = Symbol(); timers.set(id, { fn, ms }); return id; },
    unschedule: id => timers.delete(id),
    ...overrides.options,
  };
  const fire = async () => {
    const [id, timer] = [...timers.entries()][0];
    timers.delete(id); timer.fn();
    await new Promise(resolve => setImmediate(resolve));
  };
  return { timers, client, options, fire };
}

test('status errors back off exponentially to 30 s and reset after a success', async () => {
  let fail = true;
  const errors = [];
  const h = harness({ client: { refresh: async () => { if (fail) throw new Error('offline'); return {}; } }, options: { onError: e => errors.push(e) } });
  const stop = startSessionPolling(h.client, h.options);
  await new Promise(resolve => setImmediate(resolve));
  const delays = [];
  for (let i = 0; i < 7; i++) { delays.push([...h.timers.values()][0].ms); await h.fire(); }
  assert.deepEqual(delays, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  fail = false;
  await h.fire();
  assert.equal([...h.timers.values()][0].ms, 1000);
  assert.equal(errors[0], 'offline');
  stop();
  assert.equal(h.timers.size, 0);
});

test('polling pauses while the tab is hidden and resumes immediately when visible', async () => {
  const listeners = new Set();
  const document = {
    hidden: false,
    addEventListener: (type, fn) => type === 'visibilitychange' && listeners.add(fn),
    removeEventListener: (type, fn) => listeners.delete(fn),
  };
  let refreshes = 0, screens = 0;
  const h = harness({
    client: { refresh: async () => { refreshes++; return {}; }, screen: async () => { screens++; return { deviceId: 'phone' }; } },
    options: { document, screenshots: true },
  });
  const stop = startSessionPolling(h.client, h.options);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(refreshes, 1); assert.equal(screens, 1); assert.equal(h.timers.size, 2);
  document.hidden = true;
  await h.fire(); await h.fire();
  assert.equal(refreshes, 1, 'a hidden tab issues no status request');
  assert.equal(screens, 1, 'a hidden tab issues no screenshot request');
  assert.equal(h.timers.size, 0, 'no timers are scheduled while hidden');
  document.hidden = false;
  for (const fn of listeners) fn();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(refreshes, 2); assert.equal(screens, 2); assert.equal(h.timers.size, 2);
  stop();
  assert.equal(listeners.size, 0, 'stop removes the visibility listener');
  assert.equal(h.timers.size, 0);
});
