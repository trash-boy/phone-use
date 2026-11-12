// Transparent test substitute for iphone-control: same daemon/setup invocation and NDJSON socket.
import net from 'node:net';
import readline from 'node:readline';
import { appendFileSync, chmodSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

if (process.argv[2] === 'descendant') {
  process.on('SIGTERM', () => {});
  setInterval(() => {}, 1000);
  process.stdout.write(`descendantPID:${process.pid}\n`);
} else if (process.argv[2] === 'setup') {
  if (process.env.FIXTURE_DESCENDANT === 'setup') {
    const descendant = spawn(process.execPath, [fileURLToPath(import.meta.url), 'descendant'], { stdio: ['ignore', 'pipe', 'ignore'] });
    descendant.stdout.pipe(process.stdout);
  }
  if (process.env.FIXTURE_IGNORE_TERM) process.on('SIGTERM', () => {});
  if (process.env.FIXTURE_SETUP_BURST) process.stdout.write('test output '.repeat(8000));
  process.stdout.write('Apple account input: ');
  const input = readline.createInterface({ input: process.stdin });
  input.once('line', () => { process.stdout.write('Input received; setup complete\n'); input.close(); process.exit(0); });
} else if (process.argv[2] === 'daemon') {
  let taps = 0;
  let typeRequests = 0;
  let previewPrepareRequests = 0;
  let wdaUnavailableUntil = 0;
  let recoveryObservationFailures = Number(process.env.FIXTURE_RECOVERY_OBSERVATION_FAILURES || 0);
  let initialObservationFailures = Number(process.env.FIXTURE_INITIAL_OBSERVATION_WDA_FAILURES || 0);
  const counts = { screenshot: 0, list_elements: 0 };
  const sockets = new Set();
  const server = net.createServer(socket => {
    sockets.add(socket);
    socket.on('error', () => {});
    socket.on('close', () => sockets.delete(socket));
    let buffer = '';
    socket.on('data', chunk => {
      buffer += chunk;
      if (!buffer.includes('\n')) return;
      const request = JSON.parse(buffer.slice(0, buffer.indexOf('\n')));
      if (process.env.FIXTURE_REQUEST_LOG) appendFileSync(process.env.FIXTURE_REQUEST_LOG, request.op + '\n');
      if (request.op in counts) counts[request.op]++;
      if (request.op === 'observe') { counts.screenshot++; counts.list_elements++; }
      if (request.op === 'tap') taps++;
      if (request.op === 'type_text') typeRequests++;
      if (request.op === 'preview_prepare') previewPrepareRequests++;
      if (request.op === 'tap' && request.x === 999) return;
      const elementDelay = Number(process.env.FIXTURE_ELEMENTS_DELAY_MS || 0);
      const budget = request.snapshot_timeout_ms ?? 1000;
      const typingError = process.env.FIXTURE_TYPE_TEXT_ERROR || (!taps && process.env.FIXTURE_TYPE_TEXT_NEEDS_FOCUS);
      const tapFailure = request.op === 'tap' && taps <= Number(process.env.FIXTURE_TAP_WDA_FAILURES || 0);
      const initialObservationFailure = request.op === 'observe' && initialObservationFailures > 0;
      if (initialObservationFailure) initialObservationFailures--;
      if (tapFailure) wdaUnavailableUntil = Date.now() + Number(process.env.FIXTURE_WDA_RECOVERY_MS || 0);
      if (tapFailure && process.env.FIXTURE_TAP_INTERRUPTED) { socket.end(); return; }
      const recovering = Date.now() < wdaUnavailableUntil;
      const observationFailure = request.op === 'observe' && taps > 0 && (recovering || recoveryObservationFailures > 0);
      if (observationFailure && !recovering) recoveryObservationFailures--;
      const response = tapFailure || observationFailure || initialObservationFailure
        ? { kind: 'error', message: process.env.FIXTURE_TAP_ERROR || 'WDA request timed out (it was sent, the phone may still be carrying it out; take a screenshot first): WDA request timed out' }
        : request.op === 'daemon' && (process.env.FIXTURE_TAP_WDA_FAILURES || process.env.FIXTURE_INITIAL_OBSERVATION_WDA_FAILURES)
        ? { kind: 'text', text: JSON.stringify({ state: { state: recovering ? 'wda_starting' : 'ready' } }) }
        : request.op === 'type_text' && typingError
        ? { kind: 'error', message: typingError }
        : request.op === 'preview_prepare'
        ? process.env.FIXTURE_PREVIEW_PREPARE_ERROR || (process.env.FIXTURE_PREVIEW_PREPARE_ERROR_AFTER
            && previewPrepareRequests > Number(process.env.FIXTURE_PREVIEW_PREPARE_ERROR_AFTER))
          ? { kind: 'error', message: 'MJPEG settings rejected' }
          : { kind: 'text', text: JSON.stringify({ mjpegPrepared: !process.env.FIXTURE_PREVIEW_PREPARE_UNACKNOWLEDGED }) }
        : request.op === 'observe'
        ? { kind: 'text', text: JSON.stringify({ deviceId: taps && process.env.FIXTURE_RECOVERY_FOREIGN_OBSERVATION ? 'different-device' : process.env.IPHONE_UDID, imageBase64: 'aW1hZ2U=', caption: taps ? 'fixture fresh screen after action' : 'fixture points 390×844', elementsText: elementDelay > budget ? '' : JSON.stringify({ request, taps, counts }), ...(elementDelay > budget ? { elementsError: 'fixture element budget exceeded' } : {}), timings: { screenshotMs: 0, elementsMs: Math.min(elementDelay, budget), sourceBytes: 64 } }) }
        : request.op === 'screenshot'
        ? { kind: 'image', jpeg_base64: 'aW1hZ2U=', caption: 'fixture points 390×844' }
        : request.op === 'list_apps' ? { kind: 'text', text: 'Fixture App: com.example.fixture' }
        : { kind: 'text', text: JSON.stringify({ request, taps, counts, typeRequests, env: {
          ICTL_ANISETTE_URL: process.env.ICTL_ANISETTE_URL, IPHONE_UDID: process.env.IPHONE_UDID, ICTL_SOCKET: process.env.ICTL_SOCKET,
          ICTL_DATA_DIR: process.env.ICTL_DATA_DIR, ICTL_MANAGE_USBMUXD: process.env.ICTL_MANAGE_USBMUXD,
          USBMUXD_SOCKET_ADDRESS: process.env.USBMUXD_SOCKET_ADDRESS,
        } }) };
      const line = JSON.stringify(response) + '\n';
      socket.write(line.slice(0, 3));
      const delay = request.op === 'preview_prepare' ? Number(process.env.FIXTURE_PREVIEW_PREPARE_DELAY_MS || 0)
        : request.op === 'observe' ? Math.min(elementDelay, budget) : request.op === 'list_elements' ? elementDelay : 0;
      setTimeout(() => {
        if (!socket.destroyed) {
          if (request.op === 'preview_prepare' && process.env.FIXTURE_REQUEST_LOG) appendFileSync(process.env.FIXTURE_REQUEST_LOG, 'preview_prepare_ack\n');
          socket.write(line.slice(3));
        }
      }, 5 + delay);
    });
  });
  server.listen(process.env.ICTL_SOCKET, () => chmodSync(process.env.ICTL_SOCKET, 0o600));
  process.on('SIGTERM', () => { for (const socket of sockets) socket.destroy(); server.close(() => process.exit(0)); });
} else {
  process.stderr.write('fixture only supports daemon/setup\n'); process.exit(2);
}
