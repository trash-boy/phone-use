export const FrameType = Object.freeze({
  DeviceHello: 1, OpenStream: 2, OpenResult: 3, StreamData: 4, StreamClose: 5,
  InstallProgress: 6, Error: 7, Ping: 8, Pong: 9, StartPairing: 10,
  StartInstall: 11, PairRecordReady: 12,
});
export const MAX_FRAME_PAYLOAD = 1024 * 1024;

export function encodeFrame({ type, requestId = 0, streamId = 0, payload = Buffer.alloc(0) }) {
  for (const [key, value] of Object.entries({ type, requestId, streamId })) {
    if (!Number.isInteger(value) || value < 0 || value > (key === 'type' ? 255 : 0xffffffff)) {
      throw new Error(`Invalid ${key}`);
    }
  }
  const body = Buffer.from(payload);
  if (body.length > MAX_FRAME_PAYLOAD) throw new Error('Relay payload too large');
  const frame = Buffer.alloc(16 + body.length);
  frame[0] = 1;
  frame[1] = type;
  frame.writeUInt32BE(requestId, 4);
  frame.writeUInt32BE(streamId, 8);
  frame.writeUInt32BE(body.length, 12);
  body.copy(frame, 16);
  return frame;
}

export function decodeFrame(input) {
  const frame = Buffer.from(input);
  if (frame.length < 16) throw new Error('Relay frame too short');
  if (frame[0] !== 1) throw new Error('Unsupported relay version');
  const size = frame.readUInt32BE(12);
  if (size > MAX_FRAME_PAYLOAD) throw new Error('Relay payload too large');
  if (frame.length !== 16 + size) throw new Error('Relay frame length mismatch');
  if (frame[2] !== 0 || frame[3] !== 0) throw new Error('Invalid relay flags');
  return { type: frame[1], requestId: frame.readUInt32BE(4), streamId: frame.readUInt32BE(8), payload: frame.subarray(16) };
}

export const jsonPayload = (value) => Buffer.from(JSON.stringify(value));
export const parsePayload = (payload) => JSON.parse(payload.toString('utf8'));
