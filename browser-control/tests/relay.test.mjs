import test from 'node:test';
import assert from 'node:assert/strict';
import { encodeFrame, decodeFrame } from './support/relay-protocol.mjs';

test('binary relay frames preserve stream id and payload in SDK big endian format', () => {
  const frame = { type: 4, requestId: 7, streamId: 9, payload: Buffer.from([0, 255, 1]) };
  const bytes = encodeFrame(frame);
  assert.deepEqual([...bytes.subarray(0, 16)], [1, 4, 0, 0, 0, 0, 0, 7, 0, 0, 0, 9, 0, 0, 0, 3]);
  assert.deepEqual(decodeFrame(bytes), frame);
});

test('relay rejects truncated, wrong version, oversized and malformed frames', () => {
  assert.throws(() => decodeFrame(Buffer.alloc(15)), /short/);
  const bytes = encodeFrame({ type: 8, requestId: 0, streamId: 0, payload: Buffer.alloc(0) });
  bytes[0] = 2;
  assert.throws(() => decodeFrame(bytes), /version/);
  bytes[0] = 1;
  bytes.writeUInt32BE(2, 12);
  assert.throws(() => decodeFrame(bytes), /length/);
  assert.throws(() => encodeFrame({ type: 4, requestId: -1, streamId: 0, payload: Buffer.alloc(0) }), /requestId/);
  assert.throws(() => encodeFrame({ type: 4, requestId: 0, streamId: 0, payload: Buffer.alloc(1024 * 1024 + 1) }), /large/);
});
