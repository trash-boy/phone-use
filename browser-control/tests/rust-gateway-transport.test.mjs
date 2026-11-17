import assert from 'node:assert/strict';
import { once } from 'node:events';
import { stat } from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';
import * as plist from 'plist';
import WebSocket from 'ws';
import { encodeFrame, decodeFrame, FrameType as F, jsonPayload, parsePayload } from './support/relay-protocol.mjs';
import {
  contract, device, origin, token, pause, until, harness, peer, manualPeer, ready, nativeEnv,
  muxConnect, muxPacket, muxReader, modelReplyTask, rejectUpgrade,
} from './helpers/rust-gateway.mjs';

// Relay WebSocket, virtual usbmuxd, published WebUSB SDK, MJPEG preview and HTTP transport limits.

contract('transport diagnostics accept all 64 enriched streams and reject unauthenticated or private payloads', async t => {
  const app = await harness(t);
  const session = await app.session();
  const route = `/api/sessions/${session.id}/diagnostics`;
  const metrics = {schema: 1, event: 'sample', uplinkWaiters: 1, uplinkWaitCount: 3,
    uplinkWaitMaxMs: 2001, previewPauses: 1, previewCooldownMs: 29999,
    streams: Array.from({length: 64}, (_, i) => ({
    streamId: i + 1, port: 8100, readQueuedBytes: 1024, writeQueuedBytes: 1048576,
    peerWindowBytes: 131072, readQueuedFrames: 10, sendSeq: 0xffffffff, peerAck: 0xffffff00,
    inFlightBytes: 255, availableWindowBytes: 130817, writeWaiters: 1, ackPending: 1,
    windowBlocked: 0, windowBlockedMs: 0, peerAckUnchangedMs: 10000,
    readRetainedBytes: 131072, receiveWindowBytes: 0,
  }))};
  assert.ok(Buffer.byteLength(JSON.stringify(metrics)) > 16 * 1024, 'fixture must exceed the previous upload limit');
  assert.equal((await app.api(route, 'wrong-token', metrics)).status, 401);
  const response = await app.api(route, session.token, metrics);
  assert.equal(response.status, 200, await response.clone().text());
  await until(() => app.logs().split('\n').slice(0, -1).some(line => line.includes('event=browser_transport')));
  const line = app.logs().split('\n').find(line => line.includes('event=browser_transport'));
  assert.ok(line.includes(`session=${session.id}`));
  const stored = JSON.parse(line.split(' metrics=')[1]);
  assert.equal(stored.streams.length, 64);
  assert.equal(stored.streams[63].sendSeq, 0xffffffff);
  assert.equal(stored.streams[63].peerAckUnchangedMs, 10000);
  assert.equal(stored.streams[63].readRetainedBytes, 131072);
  assert.equal(stored.streams[63].receiveWindowBytes, 0);
  assert.equal(stored.previewCooldownMs, 29999);
  assert.equal(stored.uplinkWaitMaxMs, 2001);
  assert.equal((await app.api(route, session.token, {...metrics, payload: 'PRIVATE_DIAGNOSTIC_PAYLOAD'})).status, 400);
  assert.ok(!app.logs().includes('PRIVATE_DIAGNOSTIC_PAYLOAD'));
});

contract('Rust virtual mux interoperates with plist and relay, preserves coalesced raw data and USB MTU', async t => {
  const app = await harness(t), s = await app.session(), usb = await peer(t, s);
  await ready(app, s);
  const address = (await nativeEnv(app, s, 'env')).USBMUXD_SOCKET_ADDRESS;
  if (!address.includes(':')) {
    // Unix socket form: only this session's owner may connect (it carries the host pairing key).
    assert.ok(path.isAbsolute(address), address);
    assert.equal((await stat(address)).mode & 0o777, 0o600);
    assert.equal((await stat(path.dirname(address))).mode & 0o777, 0o700);
  }
  const socket = await muxConnect(t, address);
  const payload = Buffer.from(plist.build({ MessageType: 'Connect', DeviceID: 1, PortNumber: ((8100 & 255) << 8) | (8100 >>> 8) }));
  const head = Buffer.alloc(16); [16 + payload.length, 1, 8, 123].forEach((n, i) => head.writeUInt32LE(n, i * 4));
  const raw = Buffer.alloc(140_000, 47); let received = Buffer.alloc(0);
  socket.on('data', b => { received = Buffer.concat([received, b]); });
  socket.write(Buffer.concat([head, payload, raw]));
  await until(() => received.length >= 16 && received.length >= received.readUInt32LE(0) + raw.length);
  const size = received.readUInt32LE(0); assert.equal(plist.parse(received.subarray(16, size).toString()).Number, 0);
  assert.deepEqual(received.subarray(size), raw); assert.ok(usb.chunks.every(n => n <= 49116));
});

contract('published WebUSB SDK and BrowserClient resume through the Rust service and handle USB unplug', async t => {
  const sdk = await import('@limrun/device-install');
  const { BrowserClient } = await import('../web/client.mjs');
  const { fakeUSB } = await import('./fixtures/fake-usb.mjs');
  const app = await harness(t);
  const usb = fakeUSB(false, true);
  class USBEvents extends EventTarget {
    async requestDevice() { return usb.device; }
    disconnect() { const event = new Event('disconnect'); Object.defineProperty(event, 'device', { value: usb.device }); this.dispatchEvent(event); }
  }
  const events = new USBEvents();
  const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator'), originalWS = globalThis.WebSocket;
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { usb: events } });
  globalThis.WebSocket = class extends WebSocket { constructor(url) { super(url, { origin }); } };
  t.after(() => { if (originalNavigator) Object.defineProperty(globalThis, 'navigator', originalNavigator); else delete globalThis.navigator; globalThis.WebSocket = originalWS; });
  const uid = '00008140-001234567890ABCD';
  const record = { udid: uid, pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'sdk-rust-buid', HostID: 'fixture' })).toString('base64') };
  const client = new BrowserClient({ sdk: { ...sdk, getPairRecord: async () => record, putPairRecord: async () => {} }, usbEvents: events });
  t.after(() => client.disconnect());
  await client.connect({ apiBase: app.base, accessToken: token });
  const s = client.snapshot().session;
  assert.equal(client.snapshot().phase, 'connected'); assert.equal(s.deviceId, uid);
  await until(async () => (await client.refresh()).ready);
  assert.deepEqual(usb.claims[0], [6, 1]); assert.deepEqual(usb.claims.at(-1), [3, 1]);
  events.disconnect();
  assert.equal(client.snapshot().phase, 'offline');
  await until(async () => (await app.api(`/api/sessions/${s.id}`, s.token)).status === 404);
});

contract('Rust closes stalled headers and returns a bounded body timeout', async t => {
  const app = await harness(t);
  const address = new URL(app.base);
  async function stalled(request) {
    const socket = net.connect(Number(address.port), address.hostname); t.after(() => socket.destroy());
    let bytes = ''; socket.on('data', chunk => { bytes += chunk; });
    await once(socket, 'connect'); socket.write(request);
    socket.setTimeout(22000, () => socket.destroy(new Error('read deadline was not enforced')));
    await once(socket, 'close'); return bytes;
  }
  const [header, body] = await Promise.all([
    stalled('GET /api/config HTTP/1.1\r\nHost: localhost\r\n'),
    stalled(`POST /api/sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer ${token}\r\nContent-Length: 500\r\nConnection: close\r\n\r\n{`),
  ]);
  assert.ok(!header || header.includes('408'));
  assert.match(body, /408/); assert.match(body, /Request body timeout/);
});

contract('MJPEG descriptor requires session credentials and preview ticket cannot authorize commands', async t => {
  const app = await harness(t);
  const session = await app.session();
  await peer(t, session); await ready(app, session);
  const route = `/api/sessions/${session.id}`;
  assert.equal((await app.api(route + '/preview/stream', 'wrong')).status, 401);
  const descriptor = await app.api(route + '/preview/stream', session.token);
  assert.equal(descriptor.status, 200);
  const { url } = await descriptor.json();
  assert.ok(url.startsWith(route + '/preview/mjpeg/'));
  assert.ok(!url.includes(session.token));
  const ticket = url.split('/').at(-1);
  assert.equal((await app.api(route, ticket)).status, 401);
  assert.equal((await app.api(route + '/preview/mjpeg/invalid', session.token)).status, 401);
  assert.equal((await app.api(url, '', undefined, { Origin: 'https://evil.test' })).status, 403);
  await app.api(route, session.token, undefined, {}, 'DELETE');
  assert.ok([401, 404].includes((await app.api(url, '')).status));
});

contract('MJPEG preview receives independent USB frames while model is thinking and closes on disconnect', async t => {
  const fixture = await modelReplyTask(t, () => null);
  const { app, session: first } = fixture;
  await app.api(fixture.route, first.token, undefined, {}, 'DELETE');
  const s = await app.session();
  const ws = new WebSocket(s.relayUrl, { origin });
  const ports = new Map(); const intervals = [];
  const closedStreams = new Set();
  ws.on('message', bytes => {
    const f = decodeFrame(bytes);
    if (f.type === F.StreamClose) closedStreams.add(f.streamId);
    if (f.type === F.OpenStream) {
      ports.set(f.streamId, JSON.parse(f.payload).port);
      ws.send(encodeFrame({ type: F.OpenResult, requestId: f.requestId, streamId: f.streamId, payload: jsonPayload({ ok: true }) }));
    }
    if (f.type === F.StreamData && ports.get(f.streamId) === 9100) {
      assert.match(f.payload.toString(), /GET \/ HTTP/);
      const send = bytes => { if (ws.readyState === WebSocket.OPEN) ws.send(encodeFrame({ type: F.StreamData, streamId: f.streamId, payload: bytes })); };
      send(Buffer.from('HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n'));
      const image = Buffer.from([255,216,255,217]);
      const frame = Buffer.concat([Buffer.from('--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n'), image, Buffer.from('\r\n')]);
      send(frame); intervals.push(setInterval(() => send(frame), 60));
    }
  });
  t.after(() => { for (const timer of intervals) clearInterval(timer); ws.terminate(); });
  await once(ws, 'open'); ws.send(encodeFrame({ type: F.DeviceHello, payload: jsonPayload(device) }));
  await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  await app.api(route + '/tasks', s.token, { instruction: 'Inspect reference safely' });
  await until(async () => (await (await app.api(route, s.token)).json()).task?.state === 'thinking');
  const descriptor = await (await app.api(route + '/preview/stream', s.token)).json();
  const response = await fetch(app.base + descriptor.url);
  assert.equal(response.status, 200); assert.match(response.headers.get('content-type'), /multipart\/x-mixed-replace/);
  assert.equal(response.headers.get('x-accel-buffering'), 'no');
  const reader = response.body.getReader(); const received = await reader.read();
  assert.ok(Buffer.from(received.value).includes(Buffer.from([255,216,255,217])));
  const state = await (await app.api(route, s.token)).json();
  assert.equal(state.task.state, 'thinking'); assert.equal(state.preview.state, 'streaming'); assert.ok(state.preview.frames >= 1);
  assert.equal((await app.api(route + '/preview/stream', 'wrong-token', undefined, {}, 'DELETE')).status, 401);
  const stopped = await app.api(route + '/preview/stream', s.token, undefined, {}, 'DELETE');
  assert.equal(stopped.status, 200);
  await until(() => [...closedStreams].some(id => ports.get(id) === 9100));
  let ended = false;
  for (let i = 0; i < 10; i++) { if ((await reader.read()).done) { ended = true; break; } }
  assert.ok(ended, 'explicit stop must close the HTTP body even if the browser/proxy keeps it open');
  assert.equal(ws.readyState, WebSocket.OPEN);
  assert.equal((await (await app.api(route, s.token)).json()).preview.state, 'waiting');
  const resumed = await fetch(app.base + descriptor.url);
  assert.equal(resumed.status, 200);
  const resumedReader = resumed.body.getReader();
  assert.ok(Buffer.from((await resumedReader.read()).value).includes(Buffer.from([255,216,255,217])));
  ws.close(); await once(ws, 'close');
  for (let i = 0; i < 10; i++) { if ((await resumedReader.read()).done) return; }
  assert.fail('preview did not close with its owning session');
});

contract('relay WebSocket requires the session token, an exact origin and the session device before native startup', async t => {
  const app = await harness(t);
  const session = await app.session();
  assert.equal(await rejectUpgrade(session.relayUrl.replace(session.token, 'bad')), 401);
  assert.equal(await rejectUpgrade(session.relayUrl, 'http://evil'), 403);
  const wrong = new WebSocket(session.relayUrl, { origin });
  wrong.on('error', () => {});
  await once(wrong, 'open');
  wrong.send(encodeFrame({ type: F.DeviceHello, payload: jsonPayload({ ...device, serialNumber: '00008110-001234567890002E' }) }));
  await once(wrong, 'close');
  await until(async () => (await app.api(`/api/sessions/${session.id}`, session.token)).status === 404);
  assert.ok(!app.logs().includes('event=native_started'), 'a mismatched hello must not start native control');
});

contract('relay closes the session after a hello timeout or a malformed frame', async t => {
  const app = await harness(t);
  const idleSession = await app.session();
  const idle = new WebSocket(idleSession.relayUrl, { origin });
  idle.on('error', () => {}); t.after(() => idle.terminate());
  await once(idle, 'open');
  const started = Date.now();
  await once(idle, 'close');
  assert.ok(Date.now() - started < 15000);
  await until(async () => (await app.api(`/api/sessions/${idleSession.id}`, idleSession.token)).status === 404);
  const s = await app.session(); const usb = await peer(t, s); await ready(app, s);
  usb.ws.send(Buffer.alloc(3));
  await once(usb.ws, 'close');
  await until(async () => (await app.api(`/api/sessions/${s.id}`, s.token)).status === 404);
});

contract('virtual usbmuxd parses fragmented and coalesced plist messages and serves only its own device', async t => {
  const app = await harness(t), s = await app.session(); await peer(t, s); await ready(app, s);
  const socket = await muxConnect(t, (await nativeEnv(app, s)).USBMUXD_SOCKET_ADDRESS);
  const read = muxReader(socket);
  const first = muxPacket({ MessageType: 'ListDevices' }, 81);
  socket.write(first.subarray(0, 7));
  socket.write(Buffer.concat([first.subarray(7), muxPacket({ MessageType: 'Listen' }, 82)]));
  const list = await read();
  assert.equal(list.tag, 81);
  assert.equal(list.value.DeviceList.length, 1);
  assert.equal(list.value.DeviceList[0].Properties.SerialNumber, device.serialNumber);
  const listen = await read();
  assert.equal(listen.tag, 82); assert.equal(listen.value.Number, 0);
  assert.equal((await read()).value.MessageType, 'Attached');
  socket.write(muxPacket({ MessageType: 'Connect', DeviceID: 2, PortNumber: 32498 }));
  assert.equal((await read()).value.Number, 2);
});

contract('virtual usbmuxd preserves pair-record bytes, publishes saved records and isolates other device IDs', async t => {
  const app = await harness(t);
  const raw = Buffer.from(plist.build({ SystemBUID: 'initial-buid', HostID: 'host' }));
  const s = await app.session({ pairRecord: { udid: device.serialNumber, pairRecordBase64: raw.toString('base64') } });
  const usb = await manualPeer(t, s); await ready(app, s);
  const socket = await muxConnect(t, (await nativeEnv(app, s)).USBMUXD_SOCKET_ADDRESS);
  const read = muxReader(socket);
  socket.write(muxPacket({ MessageType: 'ReadBUID' }));
  assert.equal((await read()).value.BUID, 'initial-buid');
  socket.write(muxPacket({ MessageType: 'ReadPairRecord', PairRecordID: device.serialNumber }));
  assert.deepEqual(Buffer.from((await read()).value.PairRecordData), raw);
  const updated = Buffer.from(plist.build({ SystemBUID: 'new-buid', HostID: 'other-host' }));
  socket.write(muxPacket({ MessageType: 'SavePairRecord', PairRecordID: device.serialNumber, PairRecordData: updated }));
  assert.equal((await read()).value.Number, 0);
  const published = await usb.next();
  assert.equal(published.type, F.PairRecordReady);
  assert.deepEqual(parsePayload(published.payload), { udid: device.serialNumber, pairRecordBase64: updated.toString('base64') });
  socket.write(muxPacket({ MessageType: 'ReadBUID' }));
  assert.equal((await read()).value.BUID, 'new-buid');
  socket.write(muxPacket({ MessageType: 'ReadPairRecord', PairRecordID: 'another-device' }));
  assert.equal((await read()).value.Number, 2);
});

contract('virtual usbmuxd reports refused opens and drops oversized frames', async t => {
  const app = await harness(t), s = await app.session(), usb = await manualPeer(t, s); await ready(app, s);
  const socket = await muxConnect(t, (await nativeEnv(app, s)).USBMUXD_SOCKET_ADDRESS);
  const read = muxReader(socket);
  socket.write(muxPacket({ MessageType: 'Connect', DeviceID: 1, PortNumber: 32498 }));
  const open = await usb.next();
  assert.equal(open.type, F.OpenStream); assert.equal(parsePayload(open.payload).port, 62078);
  usb.send(F.OpenResult, { ok: false, error: 'refused' }, open.requestId, open.streamId);
  assert.equal((await read()).value.Number, 3);
  const header = Buffer.alloc(16);
  header.writeUInt32LE(1024 * 1024 + 17, 0);
  const closed = once(socket, 'close');
  socket.write(header);
  await closed;
  assert.equal(usb.ws.readyState, WebSocket.OPEN, 'a bad local client must not tear down the relay');
});

contract('SDK pairing publishes SavePairRecord, bridges real streams, answers Ping and refuses asset install', async t => {
  const app = await harness(t), s = await app.session(), usb = await manualPeer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  const address = (await nativeEnv(app, s)).USBMUXD_SOCKET_ADDRESS;
  usb.send(F.StartPairing, {}, 41);
  await until(async () => (await (await app.api(route, s.token)).json()).setup.state === 'running');
  const pairRecord = { udid: device.serialNumber, pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'browser-buid', HostID: 'fixture' })).toString('base64') };
  const mux = await muxConnect(t, address);
  mux.write(muxPacket({ MessageType: 'SavePairRecord', PairRecordID: device.serialNumber, PairRecordData: Buffer.from(pairRecord.pairRecordBase64, 'base64') }));
  const saved = await usb.next();
  assert.equal(saved.type, F.PairRecordReady);
  assert.deepEqual(parsePayload(saved.payload), pairRecord);
  assert.equal((await app.api(route + '/setup/input', s.token, { line: 'test-secret' })).status, 200);
  await until(async () => (await (await app.api(route, s.token)).json()).setup.state === 'complete');
  mux.destroy();
  const stream = await muxConnect(t, address);
  stream.write(muxPacket({ MessageType: 'Connect', DeviceID: 1, PortNumber: 32498 }));
  const open = await usb.next();
  assert.equal(open.type, F.OpenStream); assert.equal(parsePayload(open.payload).port, 62078);
  const result = once(stream, 'data');
  usb.send(F.OpenResult, { ok: true }, open.requestId, open.streamId);
  assert.equal(plist.parse((await result)[0].subarray(16).toString()).Number, 0);
  stream.write(Buffer.from([0, 255, 1]));
  const data = await usb.next();
  assert.equal(data.type, F.StreamData); assert.equal(data.streamId, open.streamId);
  assert.deepEqual(data.payload, Buffer.from([0, 255, 1]));
  const nativeData = once(stream, 'data');
  usb.send(F.StreamData, Buffer.from([255, 0, 2]), 0, open.streamId);
  assert.deepEqual((await nativeData)[0], Buffer.from([255, 0, 2]));
  const closed = once(stream, 'close');
  usb.send(F.StreamClose, {}, 0, open.streamId); await closed;
  usb.send(F.Ping, Buffer.alloc(0), 88);
  const pong = await usb.next();
  assert.equal(pong.type, F.Pong); assert.equal(pong.requestId, 88);
  usb.send(F.StartInstall, pairRecord, 89);
  const error = await usb.next();
  assert.equal(error.type, F.Error); assert.match(parsePayload(error.payload).error, /native setup/i);
});

contract('an unanswered stream open times out, unblocking the native Connect, and unknown stream data closes the relay', async t => {
  const app = await harness(t), s = await app.session(), usb = await manualPeer(t, s); await ready(app, s);
  const socket = await muxConnect(t, (await nativeEnv(app, s)).USBMUXD_SOCKET_ADDRESS);
  const read = muxReader(socket);
  socket.write(muxPacket({ MessageType: 'Connect', DeviceID: 1, PortNumber: 32498 }));
  const unanswered = await usb.next();
  assert.equal(unanswered.type, F.OpenStream);
  assert.equal((await read()).value.Number, 3);
  assert.equal((await usb.next()).type, F.StreamClose);
  usb.send(F.StreamData, Buffer.from('unknown'), 0, unanswered.streamId + 1);
  await once(usb.ws, 'close');
  await until(async () => (await app.api(`/api/sessions/${s.id}`, s.token)).status === 404);
});

contract('late SDK bytes for a closed stream do not terminate another active stream or the session', async t => {
  const app = await harness(t), s = await app.session(), usb = await manualPeer(t, s); await ready(app, s);
  const address = (await nativeEnv(app, s)).USBMUXD_SOCKET_ADDRESS;
  async function connectStream() {
    const native = await muxConnect(t, address);
    native.write(muxPacket({ MessageType: 'Connect', DeviceID: 1, PortNumber: 32498 }));
    const open = await usb.next();
    const result = once(native, 'data');
    usb.send(F.OpenResult, { ok: true }, open.requestId, open.streamId);
    assert.equal(plist.parse((await result)[0].subarray(16).toString()).Number, 0);
    return { native, open };
  }
  const first = await connectStream(), second = await connectStream();
  first.native.destroy();
  const closed = await usb.next();
  assert.equal(closed.type, F.StreamClose); assert.equal(closed.streamId, first.open.streamId);
  const received = once(second.native, 'data');
  usb.send(F.StreamData, Buffer.from('raced final bytes'), 0, first.open.streamId);
  usb.send(F.StreamClose, {}, 0, first.open.streamId);
  usb.send(F.StreamData, Buffer.from('still active'), 0, second.open.streamId);
  const surviving = await Promise.race([received, once(usb.ws, 'close').then(() => assert.fail('late bytes closed the relay'))]);
  assert.deepEqual(surviving[0], Buffer.from('still active'));
  usb.send(F.Ping, Buffer.alloc(0), 123);
  assert.equal((await usb.next()).type, F.Pong);
  assert.equal((await app.api(`/api/sessions/${s.id}`, s.token)).status, 200);
});

async function publishedSdkClient(t, app, usb, events) {
  const sdk = await import('@limrun/device-install');
  const { BrowserClient } = await import('../web/client.mjs');
  const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator'), originalWS = globalThis.WebSocket, originalFetch = globalThis.fetch;
  const created = [];
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { usb: events } });
  globalThis.WebSocket = class extends WebSocket { constructor(url) { super(url, { origin }); } };
  // Record the sessions the client creates, so the test can check they are gone afterwards.
  globalThis.fetch = async (url, options) => {
    const response = await originalFetch(url, options);
    if (String(url).endsWith('/api/sessions') && options?.method === 'POST' && response.status === 201) created.push(await response.clone().json());
    return response;
  };
  t.after(() => { if (originalNavigator) Object.defineProperty(globalThis, 'navigator', originalNavigator); else delete globalThis.navigator; globalThis.WebSocket = originalWS; globalThis.fetch = originalFetch; });
  const record = { udid: '00008140-001234567890ABCD', pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'fake-browser-buid', HostID: 'fake' })).toString('base64') };
  let stored;
  const client = new BrowserClient({ sdk: { ...sdk, getPairRecord: async () => record, putPairRecord: async value => { stored = value; } }, usbEvents: events });
  t.after(() => client.disconnect());
  return { client, record, created, stored: () => stored };
}

for (const [allFail, fragmentVersion] of [[false, false], [true, false], [false, true]]) {
  const name = allFail ? 'all claim failures close USB and delete the session'
    : fragmentVersion ? 'pairs when a usbmux reply spans multiple USB reads' : 'falls back from configuration 6 to 3 and pairs';
  contract(`published SDK ${name} through the Rust gateway`, async t => {
    const { fakeUSB } = await import('./fixtures/fake-usb.mjs');
    const app = await harness(t);
    const usb = fakeUSB(allFail, fragmentVersion);
    const fixture = await publishedSdkClient(t, app, usb, { requestDevice: async () => usb.device, addEventListener() {}, removeEventListener() {} });
    const gone = async () => {
      for (const s of fixture.created) await until(async () => (await app.api(`/api/sessions/${s.id}`, s.token)).status === 404);
    };
    if (allFail) {
      await assert.rejects(fixture.client.connect({ apiBase: app.base, accessToken: token }), /Unable to claim interface/);
      assert.equal(fixture.client.snapshot().phase, 'error');
      await gone();
      assert.equal(usb.device.opened, false);
      assert.deepEqual(usb.claims, [[6, 1], [6, 1], [3, 1], [3, 1]]);
      return;
    }
    await fixture.client.connect({ apiBase: app.base, accessToken: token });
    assert.equal(fixture.client.snapshot().phase, 'connected');
    assert.equal(fixture.stored().udid, fixture.record.udid);
    assert.deepEqual(usb.claims, [[6, 1], [6, 1], [3, 1]]);
    assert.equal(usb.writes.length, 2);
    assert.equal(new DataView(usb.writes[0].buffer).getUint32(8), 2);
    assert.equal(fixture.created.length, 1);
    await fixture.client.disconnect();
    await gone();
    assert.equal(usb.device.opened, false);
  });
}

contract('published SDK session survives an unrelated USB disconnect and a mux read failure, then ends on its own unplug', async t => {
  const { fakeUSB } = await import('./fixtures/fake-usb.mjs');
  const app = await harness(t);
  const usb = fakeUSB();
  class USBEvents extends EventTarget {
    listeners = new Set();
    addEventListener(type, handler, ...args) { if (type === 'disconnect') this.listeners.add(handler); super.addEventListener(type, handler, ...args); }
    removeEventListener(type, handler, ...args) { this.listeners.delete(handler); super.removeEventListener(type, handler, ...args); }
    async requestDevice() { return usb.device; }
    disconnect(selected) { const event = new Event('disconnect'); Object.defineProperty(event, 'device', { value: selected }); this.dispatchEvent(event); }
  }
  const events = new USBEvents();
  const { client, created } = await publishedSdkClient(t, app, usb, events);
  await client.connect({ apiBase: app.base, accessToken: token });
  const s = created[0];
  await until(async () => (await client.refresh()).ready);
  const native = await nativeEnv(app, s);
  events.disconnect({ serialNumber: '00008140001234567890FFFF' });
  assert.equal(client.snapshot().phase, 'connected');
  assert.equal((await app.api(`/api/sessions/${s.id}`, s.token)).status, 200, 'another device unplug keeps the session');
  usb.failRead();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.snapshot().phase, 'connected', 'SDK mux read failure alone does not revoke the WS');
  events.disconnect(usb.device);
  assert.equal(client.snapshot().phase, 'offline');
  assert.equal(client.snapshot().session, null);
  await until(async () => (await app.api(`/api/sessions/${s.id}`, s.token)).status === 404);
  assert.equal(events.listeners.size, 0);
  // Closing the session removes its private control directory and stops the native daemon process.
  await until(async () => { try { await stat(native.ICTL_SOCKET); return false; } catch { return true; } });
  const pid = Number(app.logs().match(/event=native_started [^\n]*mode=daemon pid=(\d+)/)[1]);
  await until(() => { try { process.kill(pid, 0); return false; } catch (error) { return error.code === 'ESRCH'; } });
});
