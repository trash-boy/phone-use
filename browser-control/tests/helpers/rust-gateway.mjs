// Shared harness for the Rust gateway contract suites (tests/rust-gateway-*.test.mjs).
// Each suite starts the real `iphone-control-gateway` binary (RUST_GATEWAY_BIN) with the
// native fixture in tests/fixtures/native-cli.mjs standing in for iphone-control.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
import http from 'node:http';
import * as plist from 'plist';
import WebSocket from 'ws';
import { encodeFrame, decodeFrame, FrameType as F, jsonPayload } from '../../src/relay-protocol.mjs';

export const binary = process.env.RUST_GATEWAY_BIN;
export const device = { serialNumber: '00008110-001234567890001E', vendorId: 1452, productId: 4776 };
export const origin = 'http://localhost:5173';
export const token = 'rust-contract-controller';
export const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
export const fixturePath = name => fileURLToPath(new URL(`../fixtures/${name}`, import.meta.url));
export const distPath = (...parts) => fileURLToPath(new URL(path.posix.join('../../dist', ...parts), import.meta.url));

export async function until(check, attempts = 250) {
  for (let i = 0; i < attempts; i++) { if (await check()) return; await pause(20); }
  throw new Error('condition timed out');
}

/** Starts the gateway binary. `env` reaches the process environment, `fileEnv` its `.env` file. */
export async function harness(t, env = {}, fileEnv = {}) {
  const dir = await mkdtemp(path.join(tmpdir(), 'rust-gateway-contract-'));
  await writeFile(path.join(dir, '.env'), Object.entries(fileEnv).map(([key, value]) => `${key}=${value}`).join('\n'));
  const native = path.join(dir, 'native');
  const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
  await writeFile(native, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(fixturePath('native-cli.mjs'))} "$@"\n`, { mode: 0o700 });
  const child = spawn(binary, [], { cwd: dir, env: { ...process.env, HOST: '127.0.0.1', PORT: '0', CONTROL_ACCESS_TOKEN: token,
    ALLOWED_ORIGINS: origin, RUNTIME_DIR: path.join(dir, 'runtime'), ICTL_BIN: native,
    WEB_DIST_DIR: distPath(), LLM_BASE_URL: '', LLM_MODEL: '', LLM_API_KEY: '',
    AI_OBSERVATION: 'ocr', OCR_PYTHON: process.execPath, OCR_WORKER: fixturePath('ocr-worker.mjs'), ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '', errors = '', startupError;
  child.on('error', error => { startupError = error; });
  child.stdout.on('data', chunk => { output += chunk; }); child.stderr.on('data', chunk => { errors += chunk; });
  t.after(async () => { child.kill('SIGTERM'); await Promise.race([once(child, 'exit'), pause(5000)]); if (child.exitCode === null) child.kill('SIGKILL'); await rm(dir, { recursive: true, force: true }); });
  await until(() => { if (startupError) throw startupError; if (child.exitCode !== null) throw new Error(errors); return /Gateway listening http:\/\/[^\s]+/.test(output); });
  const base = output.match(/Gateway listening (http:\/\/[^\s]+)/)[1];
  const api = (route, key = token, body, headers = {}, method = body === undefined ? 'GET' : 'POST') => fetch(base + route, {
    method, headers: { Authorization: `Bearer ${key}`, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }), ...headers },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const session = async (extra = {}) => { const response = await api('/api/sessions', token, { device, ...extra }); assert.equal(response.status, 201, await response.clone().text()); return response.json(); };
  return { api, session, base, dir, logs: () => errors, output: () => output };
}

/** A relay peer that accepts every stream open and echoes stream data back. */
export async function peer(t, session, selected = device) {
  const ws = new WebSocket(session.relayUrl, { origin });
  const chunks = [];
  ws.on('message', bytes => {
    const f = decodeFrame(bytes);
    if (f.type === F.OpenStream) ws.send(encodeFrame({ type: F.OpenResult, requestId: f.requestId, streamId: f.streamId, payload: jsonPayload({ ok: true }) }));
    if (f.type === F.StreamData) { chunks.push(f.payload.length); ws.send(encodeFrame({ type: F.StreamData, streamId: f.streamId, payload: f.payload })); }
  });
  await once(ws, 'open');
  ws.send(encodeFrame({ type: F.DeviceHello, payload: jsonPayload(selected) }));
  t.after(() => ws.terminate());
  return { ws, chunks };
}

/** A relay peer that queues every frame for the test to answer by hand. */
export async function manualPeer(t, session, selected = device) {
  const ws = new WebSocket(session.relayUrl, { origin });
  ws.on('error', () => {});
  const queued = [], waiters = [];
  ws.on('message', bytes => { const frame = decodeFrame(bytes); if (waiters.length) waiters.shift()(frame); else queued.push(frame); });
  await once(ws, 'open');
  ws.send(encodeFrame({ type: F.DeviceHello, payload: jsonPayload(selected) }));
  t.after(() => ws.terminate());
  return {
    ws,
    send(type, payload = {}, requestId = 0, streamId = 0) { ws.send(encodeFrame({ type, requestId, streamId, payload: Buffer.isBuffer(payload) ? payload : jsonPayload(payload) })); },
    next() { return queued.length ? Promise.resolve(queued.shift()) : new Promise(resolve => waiters.push(resolve)); },
  };
}

/** Resolves with the HTTP status of a refused WebSocket upgrade. */
export function rejectUpgrade(url, websocketOrigin = origin) {
  return new Promise(resolve => {
    const ws = new WebSocket(url, { origin: websocketOrigin });
    ws.on('error', () => {});
    ws.on('unexpected-response', (_, response) => { response.resume(); ws.terminate(); resolve(response.statusCode); });
  });
}

export async function ready(app, session) {
  let state;
  await until(async () => { state = await (await app.api(`/api/sessions/${session.id}`, session.token)).json(); return state.ready; });
  return state;
}

/** The environment the native fixture daemon was started with, echoed by iphone_status. */
export async function nativeEnv(app, session, requestId = 'native-env') {
  const response = await app.api(`/api/sessions/${session.id}/command`, session.token, { requestId, deviceId: session.deviceId, tool: 'iphone_status' });
  assert.equal(response.status, 200, await response.clone().text());
  return JSON.parse((await response.json()).text).env;
}

/** Connects to the session's virtual usbmuxd: `host:port` (TCP) or an absolute Unix socket path. */
export async function muxConnect(t, address) {
  let socket;
  if (address.includes(':')) {
    const [host, port] = address.split(':');
    socket = net.connect(Number(port), host);
  } else {
    socket = net.connect(address);
  }
  t.after(() => socket.destroy());
  await once(socket, 'connect');
  return socket;
}

export function muxPacket(value, tag = 7) {
  const body = Buffer.from(plist.build(value)); const header = Buffer.alloc(16);
  [body.length + 16, 1, 8, tag].forEach((n, i) => header.writeUInt32LE(n, i * 4));
  return Buffer.concat([header, body]);
}

/** Reads whole usbmuxd plist replies from a socket, tolerating fragmentation and coalescing. */
export function muxReader(socket) {
  let buffer = Buffer.alloc(0);
  const waiting = [], ready = [];
  socket.on('data', chunk => {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 16 && buffer.length >= buffer.readUInt32LE(0)) {
      const size = buffer.readUInt32LE(0);
      const result = { tag: buffer.readUInt32LE(12), value: plist.parse(buffer.subarray(16, size).toString()) };
      buffer = buffer.subarray(size);
      if (waiting.length) waiting.shift()(result); else ready.push(result);
    }
  });
  return () => ready.length ? Promise.resolve(ready.shift()) : new Promise(resolve => waiting.push(resolve));
}

/**
 * Registers a contract against the Rust gateway binary. Without RUST_GATEWAY_BIN the
 * contract fails instead of silently passing; SKIP_RUST_GATEWAY=1 skips it explicitly.
 */
export const contract = (name, fn) => {
  if (binary) return test(name, fn);
  if (process.env.SKIP_RUST_GATEWAY === '1') return test(name, { skip: 'SKIP_RUST_GATEWAY=1' }, () => {});
  return test(name, () => {
    assert.fail('RUST_GATEWAY_BIN is not set; build the gateway (npm run test:rust) or set SKIP_RUST_GATEWAY=1');
  });
};

/** Starts a gateway with a fake OpenAI-compatible provider and one running task. */
export async function modelReplyTask(t, respond, env = {}) {
  const requests = [];
  const provider = http.createServer(async (req, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    requests.push(JSON.parse(body));
    const reply = await respond(requests.length, requests.at(-1));
    if (!reply) return;
    res.writeHead(reply.status || 200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(reply.body));
  });
  await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
  t.after(() => { provider.closeAllConnections(); provider.close(); });
  const app = await harness(t, {
    LLM_BASE_URL: `http://127.0.0.1:${provider.address().port}/v1`,
    LLM_MODEL: 'fixture-replies', LLM_API_KEY: 'fixture-reply-key', ...env,
  });
  const session = await app.session();
  const usb = await peer(t, session);
  await ready(app, session);
  const route = `/api/sessions/${session.id}`;
  const response = await app.api(route + '/tasks', session.token, { instruction: 'Inspect the fixture safely' });
  assert.equal(response.status, 201);
  const task = await response.json();
  const snapshot = async () => (await app.api(route, session.token)).json();
  return { app, session, route, task, requests, snapshot, usb };
}

export function modelCompletion(content, finish_reason = 'stop') {
  return { body: { choices: [{ finish_reason, message: { content } }] } };
}

let countRequests = 0;
/** Fixture daemon counters (taps, typed text, per-op counts); each call is a fresh, undeduplicated command. */
export async function nativeCounts(fixture) {
  const response = await fixture.app.api(fixture.route + '/command', fixture.session.token, {
    requestId: `typing-counts-${++countRequests}`, deviceId: fixture.session.deviceId, tool: 'iphone_status',
  });
  return JSON.parse((await response.json()).text);
}
