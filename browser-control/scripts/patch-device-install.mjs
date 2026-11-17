// Compatibility patch for the browser ESM entry of @limrun/device-install 0.1.0.
// Keep the upstream package pinned; fail closed if its implementation changes.
// usbmux window/ACK protocol reference (behavior only; no implementation copied):
// https://github.com/libimobiledevice/usbmuxd/blob/master/src/device.c
//
// The patch is one set of exact [original, patched] snippets plus appended
// helpers, applied to the published chunk whose SHA-256 is pinned below. An
// already patched file is authenticated by reversing exactly these edits.
// Files patched by an older layout are refused: reinstall with `npm ci`.
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";

const root = new URL(
  "../node_modules/@limrun/device-install/",
  import.meta.url,
);
const metadata = JSON.parse(
  await readFile(new URL("package.json", root), "utf8"),
);
if (metadata.version !== "0.1.0")
  throw new Error(
    "Review the device-install stream patch before upgrading the SDK",
  );
const path = new URL("dist/index-DJrAlfkC.js", root);
let source = await readFile(path, "utf8");
const originalHash =
  "da410b47f3150df18f0dab904d16d22f20f0fd2141d0846c1431d4d921375a20";
const markerPrefix = "// phone-use: device-install stream lifecycle v";
const marker = markerPrefix + "7";

const replacements = [
  // Forget a partially read usbmux packet when the device is (re)opened.
  [
    `async function J(e, t) {
  const { inEndpoint: n, outEndpoint: r } = F(t), a = new Uint8Array(12), i = new DataView(a.buffer);
`,
    `async function J(e, t) {
  muxReaders.delete(e);
  const { inEndpoint: n, outEndpoint: r } = F(t), a = new Uint8Array(12), i = new DataView(a.buffer);
`,
  ],
  // Register the stream before SYN (cancellation can find it); cap streams per USB session.
  [
    `}
async function Q(e, t) {
  if (e.closed)
    throw new Error("usbmux session is closed.");
  let n, r;
`,
    `}
async function Q(e, t, onCreate) {
  if (e.closed)
    throw new Error("usbmux session is closed.");
  if (e.streams.size >= 64) throw new Error("USB stream limit exceeded");
  let n, r;
`,
  ],
  // A failed open releases its stream. Device writes go through a per-stream,
  // window-limited queue (queueDeviceWrite) instead of one unbounded write.
  [
    `  };
  return e.streams.set(g(t, a.sport), a), await w(a, m, new Uint8Array()), await a.opened, a;
}
async function K(e, t) {
  if (e.session.closed)
    throw new Error("usbmux session is closed.");
  await w(e, f, t), e.seq += t.byteLength;
}
`,
    `  };
  a.opened.catch(() => {});
  e.streams.set(g(t, a.sport), a);
  onCreate?.(a);
  try {
    await w(a, m, new Uint8Array());
    await a.opened;
    if (a.closed) throw a.error;
    return a;
  } catch (error) {
    releaseDeviceStream(a);
    throw error;
  }
}
function K(e, t) {
  return queueDeviceWrite(e, t);
}
`,
  ],
  // Session close releases every stream and notifies the relay. Segments carry an
  // explicit sequence number and advertise this stream's remaining receive window.
  [
    `function I(e) {
  e.closed = !0;
  for (const t of e.streams.values())
    for (t.error = new Error("usbmux session closed"), t.rejectOpened(t.error); t.waiters.length > 0; )
      t.waiters.shift()(new Uint8Array());
  e.streams.clear();
}
async function w(e, t, n) {
  const r = new Uint8Array(20 + n.byteLength), a = new DataView(r.buffer);
  a.setUint16(0, e.sport), a.setUint16(2, e.dport), a.setUint32(4, e.seq), a.setUint32(8, e.ack), a.setUint8(12, 80), a.setUint8(13, t), a.setUint16(14, 512), r.set(n, 20), await P(e.session, v, r);
}
`,
    `function I(e) {
  muxReaders.delete(e.device);
  e.closed = !0;
  for (const t of [...e.streams.values()])
    releaseDeviceStream(t, { notify: false, reason: "usbmux session closed" });
  e.streams.clear();
  const onClose = e.onClose;
  e.onClose = undefined;
  onClose?.();
}
async function w(e, t, n, seq = e.seq) {
  const r = new Uint8Array(20 + n.byteLength), a = new DataView(r.buffer);
  a.setUint16(0, e.sport), a.setUint16(2, e.dport), a.setUint32(4, seq), a.setUint32(8, e.ack), a.setUint8(12, 80), a.setUint8(13, t), a.setUint16(14, deviceReceiveWindow(e)), r.set(n, 20), await P(e.session, v, r);
}
`,
  ],
  // Bound the serialized USB output queue.
  [
    `    throw new Error("usbmux session is closed.");
  return e.writeChain = e.writeChain.then(async () => {
`,
    `    throw new Error("usbmux session is closed.");
  if ((e.pendingMuxWrites ?? 0) >= 256) {
    I(e);
    throw new Error("USB output queue exceeded");
  }
  e.pendingMuxWrites = (e.pendingMuxWrites ?? 0) + 1;
  return e.writeChain = e.writeChain.then(async () => {
`,
  ],
  // A failed physical write closes the USB session; release the queue slot.
  [
    `    await U(e.device, e.outEndpoint, r);
  }), e.writeChain;
}
`,
    `    await U(e.device, e.outEndpoint, r);
  }).catch(error => {
    if (!e.closed) I(e);
    throw error;
  }).finally(() => { e.pendingMuxWrites -= 1; }), e.writeChain;
}
`,
  ],
  // Reset late SYN/ACKs for abandoned streams; a device RST releases the stream.
  [
    `      const n = ne(t.payload), r = e.streams.get(g(n.sport, n.dport));
      if (r) {
        if (n.flags & G) {
          for (r.error = new Error(\`Device reset stream \${r.dport}\`), r.rejectOpened(r.error); r.waiters.length > 0; )
            r.waiters.shift()(new Uint8Array());
          e.streams.delete(g(r.dport, r.sport));
          continue;
`,
    `      const n = ne(t.payload), r = e.streams.get(g(n.sport, n.dport));
      if (!r) {
        // A cancelled SYN may still receive a late SYN/ACK. Reset it so the
        // phone does not retain a connection the server has already abandoned.
        if (!(n.flags & G)) {
          const reset = { session: e, sport: n.dport, dport: n.sport,
            seq: n.flags & f ? n.ack : 0,
            ack: n.flags & f ? 0 : n.seq + n.payload.byteLength + ((n.flags & (m | 1)) ? 1 : 0) };
          void w(reset, n.flags & f ? G : G | f, new Uint8Array()).catch(() => {});
        }
        continue;
      }
      if (r) {
        if (n.flags & G) {
          releaseDeviceStream(r, { notify: false, reason: "Device reset stream" });
          continue;
`,
  ],
  // Idempotent handshake, peer window tracking, ACKs queued off the receive loop,
  // and only contiguous data delivered (receiveDeviceData).
  [
    `        if ((n.flags & (m | f)) === (m | f)) {
          r.seq += 1, r.ack = n.seq + 1, await w(r, f, new Uint8Array()), r.resolveOpened();
          continue;
        }
        n.payload.byteLength !== 0 && (r.ack = n.seq + n.payload.byteLength, await w(r, f, new Uint8Array()), r.waiters.length > 0 ? r.waiters.shift()(n.payload) : r.queue.push(n.payload));
      }
`,
    `        if ((n.flags & (m | f)) === (m | f)) {
          if (!r.established) {
            r.seq = (r.seq + 1) >>> 0;
            r.ack = (n.seq + 1) >>> 0;
            r.established = true;
          }
          updateDeviceWindow(r, n);
          queueDeviceAck(r);
          r.resolveOpened();
          continue;
        }
        if (n.flags & f) updateDeviceWindow(r, n);
        if (n.payload.byteLength || (n.flags & 1)) receiveDeviceData(r, n);
        else if (n.seq === ((r.ack - 1) >>> 0)) queueDeviceAck(r);
      }
`,
  ],
  // USB bulk-read boundaries are not usbmux packet boundaries.
  [
    `  for (; ; ) {
    const r = await H(e, t), a = te(r, n);
    if (!(n === 1 && a.protocol !== R))
`,
    `  for (; ; ) {
    const r = await readMuxFrame(e, t, n), a = te(r, n);
    if (!(n === 1 && a.protocol !== R))
`,
  ],
  // Validate the TCP header length before slicing.
  [
    `function ne(e) {
  const t = new DataView(e.buffer, e.byteOffset, e.byteLength), n = (t.getUint8(12) >> 4) * 4;
  return {
`,
    `function ne(e) {
  if (e.byteLength < 20) throw new Error("Short usbmux TCP header");
  const t = new DataView(e.buffer, e.byteOffset, e.byteLength), n = (t.getUint8(12) >> 4) * 4;
  if (n < 20 || n > e.byteLength) throw new Error("Invalid usbmux TCP header length");
  return {
`,
  ],
  // Keep the peer's advertised window.
  [
    `    flags: t.getUint8(13),
    payload: e.slice(n)
`,
    `    flags: t.getUint8(13),
    window: t.getUint16(14),
    payload: e.slice(n)
`,
  ],
  // A closed USB transport closes the relay.
  [
    `    this.webSocketUrl = t, this.session = n, this.deviceHello = r, this.log = a;
  }
`,
    `    this.webSocketUrl = t, this.session = n, this.deviceHello = r, this.log = a;
    n.onClose = () => { this.log("USB transport closed"); this.close(); };
  }
`,
  ],
  // Never connect over a closed USB transport; release streams when the socket closes.
  [
    `  async connect() {
    const t = new WebSocket(this.webSocketUrl);
    t.binaryType = "arraybuffer", this.socket = t, t.onclose = () => {
      this.closed = !0, this.log("Relay socket closed"), this.frameQueue.finally(() => {
        this.pairRecordWaiter && (this.pairRecordWaiter.reject(new Error("Relay socket closed")), this.pairRecordWaiter = void 0);
`,
    `  async connect() {
    if (this.session.closed) throw new Error("USB transport is closed.");
    const t = new WebSocket(this.webSocketUrl);
    t.binaryType = "arraybuffer", this.socket = t, t.onclose = () => {
      this.closed = !0, releaseRelayStreams(this), this.log("Relay socket closed"), this.frameQueue.finally(() => {
        this.pairRecordWaiter && (this.pairRecordWaiter.reject(new Error("Relay socket closed")), this.pairRecordWaiter = void 0);
`,
  ],
  // Explicit close releases device streams too.
  [
    `  close() {
    this.closed = !0, this.socket?.close();
  }
`,
    `  close() {
    this.closed = !0, releaseRelayStreams(this), this.socket?.close();
  }
`,
  ],
  // Ignore frames after close; open device streams without blocking the frame queue.
  [
    `  async handleFrame(t) {
    switch (t.type) {
      case s.OpenStream:
        await this.handleOpenStream(
          t.requestId,
`,
    `  async handleFrame(t) {
    if (this.closed) return;
    switch (t.type) {
      case s.OpenStream:
        void this.handleOpenStream(
          t.requestId,
`,
  ],
  [
    `          y(t.payload).port
        );
        break;
`,
    `          y(t.payload).port
        ).catch(error => this.log("Relay stream open failed", h(error)));
        break;
`,
  ],
  // Data for cancelled streams is dropped; writes are per stream; StreamClose releases.
  [
    `        const n = this.streams.get(t.streamId);
        if (!n) throw new Error(\`Unknown stream \${t.streamId}\`);
        await K(n, t.payload);
        break;
      }
      case s.StreamClose:
        this.streams.delete(t.streamId);
        break;
`,
    `        const n = this.streams.get(t.streamId);
        if (!n || n.closed) break; // Late data after cancellation must not revive a stream.
        // Each stream owns its write queue. A zero-window stream must not
        // block Ping, cancellation or an unrelated device connection.
        void K(n, t.payload).catch(error => {
          if (this.closed || this.streams.get(t.streamId) !== n) return;
          this.streams.delete(t.streamId);
          releaseDeviceStream(n);
          this.log("Device stream write failed", h(error));
          return this.send({ type: s.StreamClose, requestId: 0, streamId: t.streamId,
            payload: l({ reason: h(error) }) });
        }).catch(error => this.log("Relay stream write failed", h(error)));
        break;
      }
      case s.StreamClose: {
        const stream = this.streams.get(t.streamId);
        this.streams.delete(t.streamId);
        if (stream) releaseDeviceStream(stream);
      }
        break;
`,
  ],
  // Heartbeats never wait behind a congested uplink.
  [
    `      case s.Ping:
        await this.send({
          type: s.Pong,
`,
    `      case s.Ping:
        void this.send({
          type: s.Pong,
`,
  ],
  [
    `          payload: new Uint8Array()
        });
        break;
`,
    `          payload: new Uint8Array()
        }).catch(error => this.log("Relay heartbeat send failed", h(error)));
        break;
`,
  ],
  // Stream limit, and the preview cooldown only for an enabled continuous viewer.
  [
    `  async handleOpenStream(t, n, r) {
    try {
      const a = await Q(this.session, r);
      this.streams.set(n, a), await this.send({
        type: s.OpenResult,
`,
    `  async handleOpenStream(t, n, r) {
    if (this.closed) return;
    if (this.streams.size >= 64) {
      await this.send({ type: s.OpenResult, requestId: t, streamId: n,
        payload: l({ ok: false, error: "USB stream limit exceeded" }) });
      return;
    }
    if (r === 9100 && this.continuousPreviewEnabled === true && performance.now() < (this.previewSuspendedUntil ?? 0)) {
      await this.send({ type: s.OpenResult, requestId: t, streamId: n,
        payload: l({ ok: false, error: "Preview paused for uplink congestion" }) });
      return;
    }
    try {
      const a = await Q(this.session, r, stream => this.streams.set(n, stream));
      if (this.closed || this.streams.get(n) !== a) return;
      await this.send({
        type: s.OpenResult,
`,
  ],
  // An open cancelled meanwhile reports nothing.
  [
    `    } catch (a) {
      this.log(
`,
    `    } catch (a) {
      if (this.closed || !this.streams.has(n)) return;
      this.streams.delete(n);
      this.log(
`,
  ],
  // Relay read credit is returned once WebSocket accepted the chunk.
  [
    `        const r = await X(n);
        if (this.closed) return;
        await this.send({
          type: s.StreamData,
          requestId: 0,
          streamId: t,
          payload: r
        });
      }
    } catch (r) {
      this.log(\`Device stream \${t} closed\`, r instanceof Error ? r.message : String(r)), await this.send({
`,
    `        const r = await X(n);
        if (this.closed || this.streams.get(t) !== n) return;
        if (!r.byteLength && n.error) throw n.error;
        try {
          await this.send({ type: s.StreamData, requestId: 0, streamId: t, payload: r });
        } finally {
          consumeDeviceRead(n, r.byteLength);
        }
      }
    } catch (r) {
      if (this.closed || this.streams.get(t) !== n) return;
      this.streams.delete(t);
      releaseDeviceStream(n);
      this.log(\`Device stream \${t} closed\`, r instanceof Error ? r.message : String(r)), await this.send({
`,
  ],
  // Bounded, back-pressured relay sends (sendBoundedRelayFrame).
  [
    `  }
  async send(t) {
    if (!this.closed) {
      if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
        this.closed = !0;
        return;
      }
      this.socket.send(_(t));
    }
  }
}
`,
    `  }
  async send(t) { await sendBoundedRelayFrame(this, t); }
}
`,
  ],
];

const helpers = `
// phone-use: device-install stream lifecycle v7
// Close both bookkeeping layers and wake readers BEFORE any asynchronous USB write.
// Local cancellation uses RST|ACK so a peer cannot retain an abandoned connection.
function releaseDeviceStream(stream, { notify = true, discard = true, reason = "Stream closed" } = {}) {
  if (stream.closed) return;
  stream.closed = true;
  stream.error = new Error(reason);
  wakeDeviceWriters(stream);
  stream.session.streams.delete(g(stream.dport, stream.sport));
  if (discard) {
    stream.queue.length = 0;
    stream.readRetainedBytes = 0;
    stream.readDiscarded = true;
  }
  stream.rejectOpened(stream.error);
  while (stream.waiters.length) stream.waiters.shift()(new Uint8Array());
  if (notify && !stream.session.closed) {
    // Consume write failures: cleanup must finish even if the USB device is gone.
    void w(stream, 20, new Uint8Array()).catch(() => {});
  }
}
function releaseRelayStreams(relay) {
  for (const stream of relay.streams.values()) releaseDeviceStream(stream);
  relay.streams.clear();
  relay.pairRecordWaiter?.reject(new Error("Relay socket closed"));
  relay.pairRecordWaiter = undefined;
}

// USB bulk-read boundaries are not usbmux packet boundaries. Retain partial
// headers/bodies and surplus packets, bounded by the 64 KiB usbmux receive limit.
const muxReaders = new WeakMap();
async function readMuxFrame(device, endpoint, version) {
  let endpoints = muxReaders.get(device);
  if (!endpoints) muxReaders.set(device, endpoints = new Map());
  let reader = endpoints.get(endpoint.endpointNumber);
  if (!reader) endpoints.set(endpoint.endpointNumber, reader = { buffer: new Uint8Array() });
  const headerLength = version >= 2 ? 16 : 8;
  for (;;) {
    const buffer = reader.buffer;
    if (buffer.byteLength >= 8) {
      const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
      const length = view.getUint32(4);
      if (length < headerLength || length > 65536) throw new Error("Invalid usbmux packet length: " + length);
      if (buffer.byteLength >= length) {
        const packet = buffer.slice(0, length);
        reader.buffer = buffer.slice(length);
        return packet;
      }
    }
    const chunk = await H(device, endpoint);
    if (!chunk.byteLength) continue; // A USB zero-length packet carries no mux data.
    const combined = new Uint8Array(reader.buffer.byteLength + chunk.byteLength);
    combined.set(reader.buffer);
    combined.set(chunk, reader.buffer.byteLength);
    reader.buffer = combined;
  }
}

// usbmux's TCP window is scaled by 256 (unlike ordinary unnegotiated TCP).
// Peer ACK/window credit is local USB flow control, separate from tunneled jktcp.
function seqAtOrBefore(a, b) { return ((b - a) >>> 0) < 0x80000000; }
function wakeDeviceWriters(stream) {
  for (const wake of stream.writeWaiters?.splice(0) ?? []) wake();
}
function updateDeviceWindow(stream, packet) {
  const previous = stream.peerAck ?? 1;
  if (!seqAtOrBefore(previous, packet.ack) || !seqAtOrBefore(packet.ack, stream.seq)) return;
  stream.peerAck = packet.ack;
  stream.peerWindow = packet.window * 256;
  wakeDeviceWriters(stream);
}
function queueDeviceWrite(stream, bytes) {
  if (stream.closed || stream.session.closed) return Promise.reject(stream.error ?? new Error("usbmux stream is closed."));
  // WebSocket has no receive backpressure API. Bound the per-stream queue and
  // fail this connection explicitly rather than retaining unlimited app data.
  const queued = (stream.queuedWriteBytes ?? 0) + bytes.byteLength;
  if (queued > 1024 * 1024) return Promise.reject(new Error("usbmux stream write queue exceeded"));
  stream.queuedWriteBytes = queued;
  const data = bytes.slice();
  const send = async () => {
    let offset = 0;
    while (offset < data.byteLength) {
      if (stream.closed || stream.session.closed) throw stream.error ?? new Error("usbmux stream is closed.");
      const inFlight = (stream.seq - (stream.peerAck ?? 1)) >>> 0;
      const available = Math.max(0, (stream.peerWindow ?? 0) - inFlight);
      if (!available) {
        await new Promise(resolve => (stream.writeWaiters ??= []).push(resolve));
        continue;
      }
      const length = Math.min(data.byteLength - offset, available, 3 * 16384 - 36);
      const part = data.subarray(offset, offset + length);
      const seq = stream.seq;
      // Reserve sequence space BEFORE the asynchronous USB write. A concurrent
      // peer data ACK must carry the sequence after all previously queued data.
      stream.seq = (seq + length) >>> 0;
      await w(stream, f, part, seq);
      offset += length;
    }
  };
  const pending = (stream.deviceWriteChain ?? Promise.resolve()).then(send);
  stream.deviceWriteChain = pending;
  return pending.finally(() => { stream.queuedWriteBytes -= data.byteLength; });
}

// Only acknowledge contiguous bytes. Retransmitted or overlapping segments must
// not corrupt Lockdown TLS, HTTP or the tunneled TCP byte stream.
function receiveDeviceData(stream, packet) {
  if (!stream.established) return;
  if (!seqAtOrBefore(packet.seq, stream.ack)) {
    queueDeviceAck(stream);
    return;
  }
  const consumed = (stream.ack - packet.seq) >>> 0;
  const payload = packet.payload.subarray(Math.min(consumed, packet.payload.byteLength));
  if (payload.byteLength) {
    const retained = (stream.readRetainedBytes ?? 0) + payload.byteLength;
    if (retained > DEVICE_READ_LIMIT) {
      // A one-byte zero-window probe is legal. Keep the cumulative ACK fixed
      // so the peer retries this byte after the consumer releases credit.
      if ((stream.readRetainedBytes ?? 0) === DEVICE_READ_LIMIT && payload.byteLength === 1) {
        queueDeviceAck(stream);
        return;
      }
      releaseDeviceStream(stream, { reason: "USB receive window exceeded" });
      return;
    }
    stream.readRetainedBytes = retained;
    stream.ack = (stream.ack + payload.byteLength) >>> 0;
    stream.waiters.length ? stream.waiters.shift()(payload) : stream.queue.push(payload);
  }
  const finished = (packet.flags & 1) && ((packet.seq + packet.payload.byteLength) >>> 0) === stream.ack;
  if (finished) {
    stream.ack = (stream.ack + 1) >>> 0;
    void w(stream, 17, new Uint8Array()).catch(() => {});
    stream.seq = (stream.seq + 1) >>> 0;
    releaseDeviceStream(stream, { notify: false, discard: false, reason: "Device finished stream" });
  } else {
    queueDeviceAck(stream);
  }
}

function queueDeviceAck(stream) {
  if (stream.closed || stream.session.closed) return;
  stream.ackNeeded = true;
  if (stream.ackWriting) return;
  stream.ackWriting = true;
  void (async () => {
    try {
      while (stream.ackNeeded && !stream.closed && !stream.session.closed) {
        stream.ackNeeded = false;
        await w(stream, f, new Uint8Array());
      }
    } catch {
      // P closes the USB session and relay on a failed physical write.
    } finally {
      stream.ackWriting = false;
    }
  })();
}

// Credit includes the chunk held by the relay pump until WebSocket accepts it.
// Shrinking only this stream's advertised window leaves USB demux/ACKs running.
const DEVICE_READ_LIMIT = 128 * 1024;
function deviceReceiveWindow(stream) {
  return Math.floor(Math.max(0, DEVICE_READ_LIMIT - (stream.readRetainedBytes ?? 0)) / 256);
}
function consumeDeviceRead(stream, length) {
  const before = deviceReceiveWindow(stream);
  stream.readRetainedBytes = Math.max(0, (stream.readRetainedBytes ?? 0) - length);
  // Update promptly on zero-window recovery; batch small credit changes to avoid
  // doubling USB ACK traffic for every tiny control response.
  const after = deviceReceiveWindow(stream);
  if ((!before && after) || after - before >= 64) queueDeviceAck(stream);
}
async function sendBoundedRelayFrame(relay, frame) {
  const data = frame.type === s.StreamData;
  const stream = data ? relay.streams.get(frame.streamId) : null;
  if (data && !stream) return;
  const preview = stream?.dport === 9100; // Reserve control capacity for ALL media, including single frames.
  // Reserve room for control responses, then a further reserve for protocol
  // messages. All checks and socket.send are synchronous once credit is free.
  const limit = preview ? 128 * 1024 : data ? 384 * 1024 : 512 * 1024;
  const bytes = _(frame);
  const started = performance.now();
  let waiting = false;
  try {
    for (;;) {
      if (relay.closed || !relay.socket || relay.socket.readyState !== WebSocket.OPEN) return;
      if (data && (relay.streams.get(frame.streamId) !== stream || stream.readDiscarded)) return;
      if ((relay.socket.bufferedAmount ?? 0) + bytes.byteLength <= limit) {
        relay.socket.send(bytes);
        return;
      }
      if (!waiting) {
        waiting = true;
        relay.uplinkWaiters = (relay.uplinkWaiters ?? 0) + 1;
        relay.uplinkWaitCount = (relay.uplinkWaitCount ?? 0) + 1;
      }
      const elapsed = performance.now() - started;
      if (preview && relay.continuousPreviewEnabled === true && elapsed >= 2000) {
        relay.previewPauses = (relay.previewPauses ?? 0) + 1;
        relay.previewSuspendedUntil = performance.now() + 30000;
        relay.log("Preview paused for uplink congestion", relay.socket.bufferedAmount ?? 0);
        throw new Error("Preview paused for uplink congestion");
      }
      if (preview && relay.continuousPreviewEnabled !== true && elapsed >= 8000) {
        throw new Error("Screenshot uplink congested");
      }
      if (elapsed >= 20000) {
        relay.log("Relay uplink stalled", relay.socket.bufferedAmount ?? 0);
        relay.close();
        throw new Error("Relay uplink stalled");
      }
      await new Promise(resolve => setTimeout(resolve, 25));
    }
  } finally {
    if (waiting) {
      relay.uplinkWaiters--;
      relay.uplinkWaitMaxMs = Math.max(relay.uplinkWaitMaxMs ?? 0, Math.round(performance.now() - started));
    }
  }
}
`;

function digest(text) {
  return createHash("sha256").update(text).digest("hex");
}
if (source.includes(marker)) {
  if (!source.endsWith(helpers))
    throw new Error("Unexpected device-install patch contents");
  source = source.slice(0, -helpers.length);
  for (const [before, after] of [...replacements].reverse()) {
    if (source.split(after).length !== 2)
      throw new Error("Unexpected device-install patched code");
    source = source.replace(after, before);
  }
} else if (source.includes(markerPrefix)) {
  throw new Error(
    "device-install was patched by an older layout; reinstall it with `npm ci`",
  );
}
if (digest(source) !== originalHash)
  throw new Error("Unexpected device-install 0.1.0 ESM checksum");
for (const [before, after] of replacements) {
  if (source.split(before).length !== 2)
    throw new Error(`device-install patch target must occur once: ${before}`);
  source = source.replace(before, after);
}
source += helpers;
await writeFile(path, source);
console.log("Verified device-install 0.1.0 browser stream lifecycle patch");
