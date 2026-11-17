import assert from 'node:assert/strict';
import { fixtureImageBase64 } from './fixtures/screen-image.mjs';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, mkdir, readdir, readFile, rm, stat, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as plist from 'plist';
import WebSocket from 'ws';
import { encodeFrame, jsonPayload, FrameType as F, parsePayload } from './support/relay-protocol.mjs';
import {
  binary, contract, device, origin, token, until, harness, peer, manualPeer, ready, nativeEnv, rejectUpgrade,
  muxConnect, muxPacket,
} from './helpers/rust-gateway.mjs';

// Session lifecycle: authentication, identities, setup, manual commands, pair-record namespaces and static files.

contract('preview avoids accessibility snapshots and observation bounds slow element reads', async t => {
  const app = await harness(t, { FIXTURE_ELEMENTS_DELAY_MS: '4000' });
  const s = await app.session(); await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  const preview = await app.api(route + '/preview', s.token);
  assert.equal(preview.status, 200);
  const frame = await preview.json(); assert.equal(frame.imageBase64, fixtureImageBase64);
  const stats = await (await app.api(route + '/command', s.token, { requestId: 'counts', deviceId: s.deviceId, tool: 'iphone_status' })).json();
  assert.deepEqual(JSON.parse(stats.text).counts, { screenshot: 1, list_elements: 0, launch_app: 0, terminate_app: 0, press_button: 0 });
  const observation = await (await app.api(route + '/screen', s.token)).json();
  assert.equal(observation.imageBase64, fixtureImageBase64); assert.ok(observation.elementsError);
  assert.ok(observation.timings.elementsMs >= 1000 && observation.timings.elementsMs < 3000, JSON.stringify(observation.timings));
  t.diagnostic(`slow 4000 ms element fixture returned after ${observation.timings.elementsMs} ms; preview used zero element reads`);
});

contract('Rust entry point preserves auth, normalized identities, setup, screenshots and command deduplication', async t => {
  const app = await harness(t);
  assert.deepEqual(await (await app.api('/api/config')).json(), { backend: 'openai', aiReady: false, model: null });
  assert.equal((await app.api('/api/sessions', 'bad', { device })).status, 401);
  assert.equal((await app.api('/api/config', token, undefined, { Origin: 'https://evil.test' })).status, 403);
  const s = await app.session({ device: { ...device, serialNumber: device.serialNumber.replace('-', '').toLowerCase() + '\0\0' } });
  assert.equal(s.deviceId, device.serialNumber);
  await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  const command = { requestId: 'once', deviceId: s.deviceId, tool: 'iphone_tap', args: { x: 5, y: 7 } };
  const responses = await Promise.all(Array.from({ length: 5 }, () => app.api(route + '/command', s.token, command).then(r => r.json())));
  assert.ok(responses.every(r => JSON.parse(r.text).taps === 1));
  const echo = JSON.parse(responses[0].text);
  assert.equal(echo.request.expect_udid, device.serialNumber); assert.equal(echo.request.cancel_on_close, true); assert.equal(echo.request.lang, 'en');
  assert.equal(echo.env.ICTL_MANAGE_USBMUXD, '0');
  assert.equal((await app.api(route + '/command', s.token, { ...command, args: { x: 6, y: 7 } })).status, 409);
  assert.equal((await app.api(route + '/command', s.token, { ...command, requestId: 'foreign', deviceId: 'a'.repeat(40) })).status, 409);
  assert.equal((await app.api(route + '/command', s.token, { ...command, requestId: 'invalid', args: { x: 1, y: 2, extra: true } })).status, 400);
  const image = await (await app.api(route + '/screen', s.token)).json(); assert.equal(image.imageBase64, fixtureImageBase64); assert.equal(image.deviceId, device.serialNumber);
  assert.equal((await app.api(route + '/setup', s.token, {})).status, 200);
  await until(async () => (await (await app.api(route, s.token)).json()).setup.transcript.includes('Apple account input'));
  assert.equal((await app.api(route + '/setup/input', s.token, { line: 'private-fixture-input' })).status, 200);
  await until(async () => (await (await app.api(route, s.token)).json()).setup.state === 'complete');
  assert.ok(!(await (await app.api(route, s.token)).text()).includes('private-fixture-input'));
  assert.equal((await app.api(route, s.token, undefined, {}, 'DELETE')).status, 200);
  assert.equal((await app.api(route, s.token)).status, 404);
});

contract('Rust resumes signing namespace exclusively and disconnect revokes tokens', async t => {
  const app = await harness(t);
  const pairRecord = { udid: device.serialNumber, pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'contract-buid' })).toString('base64') };
  const s = await app.session({ pairRecord }), usb = await peer(t, s); await ready(app, s);
  assert.equal((await app.api('/api/sessions', token, { device, pairRecord })).status, 409);
  usb.ws.close(); await once(usb.ws, 'close');
  await until(async () => (await app.api(`/api/sessions/${s.id}`, s.token)).status === 404);
  const resumed = await app.session({ pairRecord }); await peer(t, resumed); await ready(app, resumed);
  assert.notEqual(resumed.id, s.id);
});

contract('Rust serves built frontend and excludes secrets, traversal and unsupported files', async t => {
  const app = await harness(t);
  const page = await fetch(app.base + '/'); assert.equal(page.status, 200); assert.match(page.headers.get('content-type'), /text\/html/);
  const html = await page.text(); assert.match(html, /assets\//);
  const asset = html.match(/src="([^"]+\.js)"/)[1]; assert.equal((await fetch(app.base + asset)).status, 200);
  const head = await fetch(app.base + '/', { method: 'HEAD' }); assert.equal(head.status, 200); assert.equal(await head.text(), '');
  for (const url of ['/.env', '/%2eenv', '/assets/missing.js', '/api/missing', '/%2e%2e%2fCargo.toml']) assert.equal((await fetch(app.base + url)).status, 404, url);
});

contract('Rust .env native overrides reach daemon and command results remain retired after cache eviction', async t => {
  const app = await harness(t, {}, { ICTL_ANISETTE_URL: 'https://selfhosted.fixture.invalid/' });
  const s = await app.session(); await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}/command`;
  const command = { deviceId: s.deviceId, tool: 'iphone_status' };
  const first = await (await app.api(route, s.token, { ...command, requestId: 'original' })).json();
  assert.equal(JSON.parse(first.text).env.ICTL_ANISETTE_URL, 'https://selfhosted.fixture.invalid/');
  for (let i = 0; i < 256; i++) assert.equal((await app.api(route, s.token, { ...command, requestId: `evict-${i}` })).status, 200);
  const expired = await app.api(route, s.token, { ...command, requestId: 'original' });
  assert.equal(expired.status, 409); assert.match((await expired.json()).error, /cannot be replayed/);
});

contract('Rust serves WASM GET and HEAD with streaming MIME and rejects missing modules', async t => {
  const app = await harness(t);
  const files = await readdir(fileURLToPath(new URL('../dist/assets', import.meta.url)));
  const wasm = files.find(name => name.endsWith('.wasm'));
  assert.ok(wasm, 'production build must contain a real WASM module');
  const route = '/assets/' + wasm;
  const response = await fetch(app.base + route);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get('content-type'), 'application/wasm');
  const bytes = new Uint8Array(await response.arrayBuffer());
  assert.deepEqual([...bytes.slice(0, 4)], [0, 97, 115, 109]);
  const head = await fetch(app.base + route, { method: 'HEAD' });
  assert.equal(head.headers.get('content-type'), 'application/wasm');
  assert.equal(Number(head.headers.get('content-length')), bytes.length);
  assert.equal(await head.text(), '');
  assert.equal((await fetch(app.base + '/assets/missing.wasm')).status, 404);
});

contract('session creation authenticates, checks the exact origin and validates the device and body', async t => {
  const app = await harness(t);
  assert.equal((await app.api('/api/sessions', 'bad', { device })).status, 401);
  assert.equal((await app.api('/api/sessions', token, { device }, { Origin: 'http://evil' })).status, 403);
  assert.equal((await app.api('/api/sessions', token, { device: { ...device, vendorId: 1 } })).status, 400);
  assert.equal((await app.api('/api/sessions', token, { device: { ...device, serialNumber: '../../escape' } })).status, 400);
  assert.equal((await app.api('/api/sessions', token, null)).status, 400);
  assert.ok(!app.logs().includes('event=native_started'));
});

contract('two sessions get isolated native state, sockets and tokens and close independently', async t => {
  const app = await harness(t);
  const otherDevice = { ...device, serialNumber: '00008110-001234567890002E' };
  const first = await app.session(), second = await app.session({ device: otherDevice });
  const a = await peer(t, first); await peer(t, second, otherDevice);
  await ready(app, first); await ready(app, second);
  const [envA, envB] = [await nativeEnv(app, first), await nativeEnv(app, second)];
  assert.equal(envA.IPHONE_UDID, device.serialNumber); assert.equal(envB.IPHONE_UDID, otherDevice.serialNumber);
  for (const key of ['ICTL_SOCKET', 'USBMUXD_SOCKET_ADDRESS', 'ICTL_DATA_DIR']) assert.notEqual(envA[key], envB[key], key);
  for (const env of [envA, envB]) {
    assert.ok(Buffer.byteLength(env.ICTL_SOCKET) < 104, 'Unix socket paths must fit sockaddr_un');
    assert.equal((await stat(env.ICTL_DATA_DIR)).mode & 0o777, 0o700);
    assert.equal((await stat(path.dirname(env.ICTL_SOCKET))).mode & 0o777, 0o700);
  }
  assert.equal(await rejectUpgrade(first.relayUrl), 409, 'a second relay for the same session is refused');
  assert.equal((await app.api(`/api/sessions/${first.id}`, second.token)).status, 401);
  const route = `/api/sessions/${first.id}/command`;
  assert.equal((await app.api(route, first.token, { requestId: 'power', deviceId: first.deviceId, tool: 'iphone_press_button', args: { button: 'power' } })).status, 400);
  const screen = await (await app.api(`/api/sessions/${first.id}/screen`, first.token)).json();
  assert.equal(screen.imageBase64, fixtureImageBase64);
  assert.equal(screen.caption, 'fixture points 390×844');
  assert.equal(JSON.parse(screen.elementsText).request.op, 'observe');
  assert.equal(JSON.parse(screen.elementsText).request.limit, 80);
  a.ws.close(); await once(a.ws, 'close');
  await until(async () => (await app.api(`/api/sessions/${first.id}`, first.token)).status === 404);
  assert.equal((await app.api(`/api/sessions/${second.id}/command`, second.token, { requestId: 'alive', deviceId: second.deviceId, tool: 'iphone_status' })).status, 200);
});

for (const padding of ['', '\0'.repeat(16)]) {
  contract(`raw24${padding ? ' NUL-padded' : ''} identities normalize across hello, commands and pair-record resume without signing again`, async t => {
    const app = await harness(t);
    const pairRecord = { udid: device.serialNumber, pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'browser-buid', HostID: 'fixture' })).toString('base64') };
    const rawDevice = { ...device, serialNumber: device.serialNumber.replace('-', '').toLowerCase() + padding };
    const first = await app.session({ device: rawDevice, pairRecord: { ...pairRecord, udid: rawDevice.serialNumber } });
    assert.equal(first.deviceId, device.serialNumber);
    const usb = await manualPeer(t, first, rawDevice); await ready(app, first);
    const command = await app.api(`/api/sessions/${first.id}/command`, first.token, { requestId: 'raw-device', deviceId: rawDevice.serialNumber, tool: 'iphone_status' });
    assert.equal(command.status, 200);
    const echo = JSON.parse((await command.json()).text);
    assert.equal(echo.request.expect_udid, device.serialNumber);
    assert.equal(echo.env.IPHONE_UDID, device.serialNumber);
    usb.send(F.StartPairing, {}, 5);
    const resumed = await usb.next();
    assert.equal(resumed.type, F.PairRecordReady);
    assert.deepEqual(parsePayload(resumed.payload), pairRecord);
    assert.equal((await (await app.api(`/api/sessions/${first.id}`, first.token)).json()).setup.state, 'idle', 'a stored record needs no signing');
    assert.equal((await app.api('/api/sessions', token, { device, pairRecord })).status, 409, 'the namespace has an active owner');
    usb.ws.close(); await once(usb.ws, 'close');
    await until(async () => (await app.api(`/api/sessions/${first.id}`, first.token)).status === 404);
    const second = await app.session({ device, pairRecord: { ...pairRecord, udid: rawDevice.serialNumber } });
    const usb2 = await manualPeer(t, second); await ready(app, second);
    assert.equal((await nativeEnv(app, second)).ICTL_DATA_DIR, echo.env.ICTL_DATA_DIR);
    usb2.send(F.StartPairing, {}, 6);
    assert.deepEqual(parsePayload((await usb2.next()).payload), pairRecord);
    const mapDir = path.join(app.dir, 'runtime', 'record-map');
    for (const file of await readdir(mapDir)) {
      const map = await readFile(path.join(mapDir, file), 'utf8');
      assert.ok(!map.includes(pairRecord.pairRecordBase64)); assert.ok(!map.includes(token));
    }
  });
}

contract('concurrent first use of one pair record reserves a single native namespace', async t => {
  const app = await harness(t);
  const pairRecord = { udid: device.serialNumber, pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'race-buid' })).toString('base64') };
  const responses = await Promise.all([app.api('/api/sessions', token, { device, pairRecord }), app.api('/api/sessions', token, { device, pairRecord })]);
  assert.deepEqual(responses.map(r => r.status).sort(), [201, 409]);
});

contract('SavePairRecord cannot take over a mapping owned by another active session', async t => {
  const app = await harness(t);
  const pairRecord = { udid: device.serialNumber, pairRecordBase64: Buffer.from(plist.build({ SystemBUID: 'owned-buid', HostID: 'fixture' })).toString('base64') };
  const owner = await app.session({ pairRecord }); await peer(t, owner); await ready(app, owner);
  const other = await app.session(); const usb = await manualPeer(t, other); await ready(app, other);
  const mux = await muxConnect(t, (await nativeEnv(app, other)).USBMUXD_SOCKET_ADDRESS);
  mux.write(muxPacket({ MessageType: 'SavePairRecord', PairRecordID: device.serialNumber, PairRecordData: Buffer.from(pairRecord.pairRecordBase64, 'base64') }));
  await until(async () => (await app.api(`/api/sessions/${other.id}`, other.token)).status === 404);
  assert.equal((await app.api(`/api/sessions/${owner.id}`, owner.token)).status, 200);
  await until(() => usb.ws.readyState === WebSocket.CLOSED);
});

contract('setup input requires a running setup and a single line', async t => {
  const app = await harness(t), s = await app.session(); await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  assert.equal((await app.api(route + '/setup/input', s.token, { line: 'no prompt' })).status, 409);
  assert.equal((await app.api(route + '/setup', s.token, {})).status, 200);
  assert.equal((await app.api(route + '/setup/input', s.token, { line: 'first\nsecond' })).status, 400);
});

contract('setup transcript stays bounded and closing the session kills setup descendants', async t => {
  const app = await harness(t, { FIXTURE_SETUP_BURST: '1', FIXTURE_DESCENDANT: 'setup' });
  const s = await app.session(); await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  assert.equal((await app.api(route + '/setup', s.token, {})).status, 200);
  let setup;
  await until(async () => { setup = (await (await app.api(route, s.token)).json()).setup; return /descendantPID:\d+/.test(setup.transcript) && setup.transcript.includes('Apple account input'); });
  assert.ok(Buffer.byteLength(setup.transcript) <= 64 * 1024, `transcript is ${Buffer.byteLength(setup.transcript)} bytes`);
  const pid = Number(setup.transcript.match(/descendantPID:(\d+)/)[1]);
  const alive = () => { try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } };
  t.after(() => { if (alive()) process.kill(pid, 'SIGKILL'); });
  assert.equal(alive(), true);
  assert.equal((await app.api(route, s.token, undefined, {}, 'DELETE')).status, 200);
  await until(() => !alive());
});

contract('relay URL follows the configured HTTPS public base URL', async t => {
  const app = await harness(t, { PUBLIC_BASE_URL: 'https://control.example.test' });
  const s = await app.session();
  assert.equal(new URL(s.relayUrl).origin, 'wss://control.example.test');
  assert.equal(new URL(s.relayUrl).pathname, '/ios/device/ws');
});

contract('gateway refuses to start without a controller secret and never prints it', async t => {
  const dir = await mkdtemp(path.join(tmpdir(), 'rust-gateway-entry-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const missing = spawn(binary, [], { cwd: dir, env: { ...process.env, CONTROL_ACCESS_TOKEN: '', PORT: '0', RUNTIME_DIR: path.join(dir, 'runtime') } });
  let stderr = ''; missing.stderr.on('data', bytes => { stderr += bytes; });
  const [code] = await once(missing, 'exit');
  assert.notEqual(code, 0); assert.match(stderr, /CONTROL_ACCESS_TOKEN required/);
  const app = await harness(t);
  assert.deepEqual(await (await fetch(app.base + '/api/config')).json(), { backend: 'openai', aiReady: false, model: null });
  assert.ok(!app.output().includes(token) && !app.logs().includes(token));
});

contract('unconfigured AI rejects tasks explicitly while manual control stays available', async t => {
  const app = await harness(t), s = await app.session(); await peer(t, s); await ready(app, s);
  const result = await app.api(`/api/sessions/${s.id}/tasks`, s.token, { instruction: 'go' });
  assert.equal(result.status, 503); assert.match((await result.json()).error, /AI is not configured/);
  assert.equal((await app.api(`/api/sessions/${s.id}/command`, s.token, { requestId: 'manual-no-ai', deviceId: s.deviceId, tool: 'iphone_status' })).status, 200);
});

contract('static files serve the SPA through GET and HEAD and refuse symlinks, traversal and API paths', async t => {
  const root = await mkdtemp(path.join(tmpdir(), 'rust-gateway-static-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(path.join(root, 'assets'));
  await writeFile(path.join(root, 'index.html'), '<title>Console</title>');
  await writeFile(path.join(root, 'assets', 'app.js'), 'export {}');
  await symlink('/etc/passwd', path.join(root, 'leak.txt'));
  const app = await harness(t, { WEB_DIST_DIR: root });
  for (const route of ['/', '/console/session']) {
    const response = await fetch(app.base + route);
    assert.equal(response.status, 200, route); assert.match(await response.text(), /Console/);
  }
  const head = await fetch(app.base + '/', { method: 'HEAD' });
  assert.equal(head.status, 200); assert.equal(await head.text(), '');
  assert.equal((await fetch(app.base + '/assets/app.js')).headers.get('content-type'), 'text/javascript; charset=utf-8');
  for (const route of ['/assets/missing.js', '/leak.txt', '/%2e%2e%2fsecret', '/api/missing']) assert.equal((await fetch(app.base + route)).status, 404, route);
  assert.notEqual((await fetch(app.base + '/ios/device/ws')).status, 200, 'the relay route is never a static file');
  assert.equal((await fetch(app.base + '/api/sessions', { method: 'POST', body: '{}' })).status, 401);
  assert.equal((await fetch(app.base + '/api/config')).status, 200);
});

contract('binary screen route returns the latest frame bytes with session auth and no caching', async t => {
  const app = await harness(t), s = await app.session();
  const route = `/api/sessions/${s.id}/screen.jpg`;
  assert.equal((await app.api(route, 'wrong-token')).status, 401);
  assert.equal((await app.api(route, token)).status, 401, 'the controller secret is not a session credential');
  await peer(t, s); await ready(app, s);
  const response = await app.api(route, s.token);
  assert.equal(response.status, 200, await response.clone().text());
  assert.equal(response.headers.get('cache-control'), 'no-store');
  // The fixture frame is a PNG; real captures are JPEG and are labelled image/jpeg.
  assert.equal(response.headers.get('content-type'), 'image/png');
  assert.deepEqual(Buffer.from(await response.arrayBuffer()), Buffer.from(fixtureImageBase64, 'base64'));
  // The JSON preview is unchanged and shows the same frame.
  const preview = await (await app.api(`/api/sessions/${s.id}/preview`, s.token)).json();
  assert.equal(preview.imageBase64, fixtureImageBase64);
  assert.equal((await app.api(`/api/sessions/${s.id}`, s.token, undefined, {}, 'DELETE')).status, 200);
  assert.equal((await app.api(route, s.token)).status, 404);
});

contract('relay accepts the session token from an Authorization header or a subprotocol instead of the URL', async t => {
  const app = await harness(t);
  const bare = url => { const u = new URL(url); u.searchParams.delete('token'); return u.toString(); };
  const hello = ws => ws.send(encodeFrame({ type: F.DeviceHello, payload: jsonPayload(device) }));
  const viaHeader = await app.session();
  const header = new WebSocket(bare(viaHeader.relayUrl), { origin, headers: { Authorization: `Bearer ${viaHeader.token}` } });
  t.after(() => header.terminate());
  await once(header, 'open'); hello(header); await ready(app, viaHeader);
  await (await app.api(`/api/sessions/${viaHeader.id}`, viaHeader.token, undefined, {}, 'DELETE')).text();
  const viaProtocol = await app.session();
  const protocol = new WebSocket(bare(viaProtocol.relayUrl), ['ictl.relay', `ictl.token.${viaProtocol.token}`], { origin });
  t.after(() => protocol.terminate());
  await once(protocol, 'open');
  assert.equal(protocol.protocol, 'ictl.relay');
  hello(protocol); await ready(app, viaProtocol);
  const wrong = new WebSocket(bare(viaProtocol.relayUrl), ['ictl.relay', 'ictl.token.wrong'], { origin });
  const [, response] = await once(wrong, 'unexpected-response');
  assert.equal(response.statusCode, 401);
});
