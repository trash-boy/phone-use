import test from "node:test";
import assert from "node:assert/strict";
import { AndroidCommands, androidUID, mapPoint, parseHierarchy, quote } from "../web/android/core.mjs";
import { AndroidRelay, encodeFrame, decodeFrame } from "../web/android/relay.mjs";
import { encodeFrame as nodeEncode, decodeFrame as nodeDecode } from "./support/relay-protocol.mjs";
import { BrowserClient } from "../web/client.mjs";
const turn = () => new Promise((resolve) => setImmediate(resolve));
const signal = () => new AbortController().signal;
const xml = '<hierarchy rotation="0"><node text="搜索 &amp; 设置" content-desc="中文" resource-id="com.android.settings:id/search" class="android.widget.TextView" enabled="true" bounds="[10,20][210,120]"/><node text="hidden" visible-to-user="false" bounds="[0,0][1,1]"/></hierarchy>';
function fixture(overrides = {}) {
  const calls = [];
  const adapter = new AndroidCommands({ deviceId: "android:ABC", delay: async () => {},
    controller: { setClipboard: async (value) => calls.push(["clipboard", value]) },
    shell: async (command) => { calls.push(["shell", command]); return command === "dumpsys input" ? 'Viewport INTERNAL: displayId=0, valid=true, orientation=0, logicalFrame=[0, 0, 1080, 2400]' : xml; },
    capture: async () => ({ imageBase64: "anBlZw==", caption: "Android", view: { width: 576, height: 1280, nativeWidth: 1080, nativeHeight: 2400, orientation: 0 }, sourceBytes: 1000000, uploadedBytes: 90000 }), ...overrides });
  return { adapter, calls };
}
test("Android serial namespaces preserve case and cannot alias an Apple UID or path", () => {
  assert.equal(androidUID("ABC_1\0"), "android:ABC_1");
  assert.equal(androidUID("android:ABC_1"), "android:ABC_1");
  for (const value of ["", "../bad", "bad serial", "abc\0def", "x".repeat(129)]) assert.throws(() => androidUID(value));
});
test("Android hierarchy preserves Chinese labels, decodes entities, filters invisible nodes and maps scaled points", () => {
  const nodes = parseHierarchy(xml); assert.equal(nodes.length, 1); assert.equal(nodes[0].text, "搜索 & 设置");
  assert.deepEqual(mapPoint({ width: 576, height: 1280, nativeWidth: 1080, nativeHeight: 2400 }, 288, 640), [540, 1200]);
  assert.throws(() => mapPoint(null, 0, 0)); assert.throws(() => mapPoint({ width: 10, height: 10 }, 10, 0));
  assert.throws(() => parseHierarchy('ERROR: null root node'));
  assert.equal(quote("https://test/?q=';$(touch /bad)"), "'https://test/?q='\\'';$(touch /bad)'");
});
test("Android observation sends compressed screenshot and scaled elements; unavailable tree still yields usable image", async () => {
  const { adapter } = fixture();
  const result = JSON.parse((await adapter.execute({ op: "observe", limit: 80 }, signal())).text);
  assert.equal(result.deviceId, "android:ABC"); assert.equal(result.imageBase64, "anBlZw==");
  assert.deepEqual(JSON.parse(result.elementsText).bounds, [5, 11, 107, 53]);
  const { adapter: fallback } = fixture({ shell: async () => { throw new Error("UI busy"); } });
  const partial = JSON.parse((await fallback.execute({ op: "observe" }, signal())).text);
  assert.equal(partial.imageBase64, result.imageBase64); assert.match(partial.elementsError, /unavailable/);
});
test("Android screenshot metadata reaches observations while zero element limits skip hierarchy reads", async () => {
  const coordinateMapping = {offsetX: 1.25, offsetY: 2.5, scaleX: 10.6 / 11, scaleY: 20.4 / 20};
  const {adapter, calls} = fixture({capture: async () => ({
    imageBase64: "anBlZw==", caption: "Android crop", coordinateMapping,
    view: {width: 576, height: 1280, nativeWidth: 1080, nativeHeight: 2400, orientation: 0},
  })});
  const image = await adapter.execute({op: "screenshot"}, signal());
  assert.deepEqual(image.coordinateMapping, coordinateMapping);
  const observation = JSON.parse((await adapter.execute({op: "observe", limit: 0, snapshot_timeout_ms: 0}, signal())).text);
  assert.deepEqual(observation.coordinateMapping, coordinateMapping);
  assert.equal(observation.elementsText, "");
  assert.equal(observation.timings.elementsMs, 0);
  assert.equal(observation.elementsError, undefined);
  assert.equal(calls.length, 0, "OCR-only observations must skip uiautomator");
  const manual = JSON.parse((await adapter.execute({op: "observe", limit: 80}, signal())).text);
  assert.notEqual(manual.elementsText, "");
  assert.equal(calls.filter(call => call[1].startsWith("uiautomator dump")).length, 1);
});

test("Android capture returns exact crop mapping after independently rounding output dimensions", async (t) => {
  const wasm = (await import('../web/wasm.mjs')).default;
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'OffscreenCanvas');
  const draws = [];
  Object.defineProperty(globalThis, 'OffscreenCanvas', {configurable: true, value: class {
    getContext() { return {drawImage(...args) { draws.push(args); }}; }
    async convertToBlob() { return {size: 3, arrayBuffer: async () => Uint8Array.of(1, 2, 3).buffer}; }
  }});
  t.after(() => {
    if (previous) Object.defineProperty(globalThis, 'OffscreenCanvas', previous);
    else delete globalThis.OffscreenCanvas;
  });
  let closed = 0;
  const video = {snapshot: async () => ({
    bitmap: {width: 1080, height: 2400, close() { closed++; }},
    viewport: {width: 1080, height: 2400, orientation: 0}, sourceFormat: "video", sourceBytes: 999,
  })};
  const full = await wasm.captureAndroid({}, signal(), undefined, undefined, video);
  assert.deepEqual(full.coordinateMapping, {offsetX: 0, offsetY: 0, scaleX: 1, scaleY: 1});
  const crop = await wasm.captureAndroid({}, signal(), [1.25, 2.5, 10.6, 20.4], full.view, video);
  assert.deepEqual(crop.coordinateMapping, {offsetX: 1.25, offsetY: 2.5, scaleX: 10.6 / 11, scaleY: 20.4 / 20});
  assert.deepEqual(draws[1].slice(-4), [0, 0, 11, 20]);
  assert.deepEqual(draws[1].slice(1, 5), [1.25 * 1080 / 576, 2.5 * 2400 / 1280, 10.6 * 1080 / 576, 20.4 * 2400 / 1280]);
  assert.equal(closed, 2);
});

test("Android coordinate actions require a current screenshot and reject rotation before sending any input", async () => {
  const { adapter, calls } = fixture();
  await assert.rejects(adapter.execute({ op: "tap", x: 10, y: 10 }, signal()), /screenshot/);
  await adapter.execute({ op: "screenshot" }, signal());
  await adapter.execute({ op: "tap", x: 288, y: 640 }, signal());
  assert.ok(calls.some((c) => c[1] === "input tap 540 1200"));
  adapter.shell = async () => 'Viewport INTERNAL: displayId=0, valid=true, orientation=2, logicalFrame=[0, 0, 1080, 2400]';
  await assert.rejects(adapter.execute({ op: "tap", x: 288, y: 640 }, signal()), /changed/);
  adapter.shell = async () => 'Viewport INTERNAL: displayId=0, valid=true, orientation=0, logicalFrame=[0, 0, 2400, 1080]';
  const length = calls.length;
  await assert.rejects(adapter.execute({ op: "swipe", from: [1, 1], to: [2, 2] }, signal()), /changed/);
  assert.equal(calls.length, length);
});
test("Android Unicode paste uses clipboard ack, submit is separate, and a missing semantic target reports tapped_nothing", async () => {
  const { adapter, calls } = fixture();
  await adapter.execute({ op: "type_text", text: "你好，Android 👋", submit: true }, signal());
  assert.deepEqual(calls[0], ["clipboard", { content: "你好，Android 👋", paste: true, sequence: 1n }]);
  assert.deepEqual(calls[1], ["shell", "input keyevent 66"]);
  const result = await adapter.execute({ op: "tap_element", text: "not present" }, signal());
  assert.deepEqual(result.outcome, { tapped_nothing: true });
  await assert.rejects(adapter.execute({ op: "launch_app", app: "x; touch /bad" }, signal()));
});
class Socket extends EventTarget {
  sent = []; send(bytes) { this.sent.push(decodeFrame(bytes)); } close() { this.closed = true; }
  receive(type, id, value) { this.dispatchEvent(new MessageEvent("message", { data: encodeFrame(type, id, value).buffer })); }
}
function relayFixture(execute) {
  const socket = new Socket(); const relay = new AndroidRelay(socket, "android:ABC", execute);
  socket.receive(17, 0, { ready: true, deviceId: "android:ABC" });
  return { socket, relay };
}
test("Android relay wire format agrees with Rust/iOS framing and rejects oversized or mismatched frames", () => {
  const frame = encodeFrame(14, 7, { text: "你好" });
  const node = nodeDecode(frame);
  assert.equal(node.requestId, 7); assert.equal(node.type, 14);
  assert.deepEqual(decodeFrame(nodeEncode({ type: 15, requestId: 9, payload: Buffer.from('{"ok":true}') })), { type: 15, requestId: 9, value: { ok: true } });
  frame[2] = 1; assert.throws(() => decodeFrame(frame));
  assert.throws(() => encodeFrame(14, 1, "x".repeat(1024 * 1024)));
});
test("Android relay serializes actions, cancels queued work, and rejects duplicate action IDs without replay", async () => {
  let release; const gate = new Promise((resolve) => { release = resolve; }); const calls = [];
  const { socket, relay } = relayFixture(async (request) => { calls.push(request.op); if (request.op === "first") await gate; return { kind: "text", text: "ok" }; });
  await relay.ready;
  socket.receive(14, 1, { deviceId: "android:ABC", request: { op: "first" } });
  socket.receive(14, 2, { deviceId: "android:ABC", request: { op: "cancelled" } });
  socket.receive(16, 2, null); await turn(); release(); await relay.chain;
  assert.deepEqual(calls, ["first"]); assert.deepEqual(socket.sent.map((f) => f.requestId), [1]);
  socket.receive(14, 1, { deviceId: "android:ABC", request: { op: "first" } });
  assert.equal(relay.closed, true); assert.deepEqual(calls, ["first"]);
});
test("Android cancellation frees relay queue even if an input ack is stalled", async () => {
  let started; const start = new Promise((resolve) => { started = resolve; }); const calls = [];
  const { socket, relay } = relayFixture(async (r) => { calls.push(r.op); if (r.op === "stalled") { started(); await new Promise(() => {}); } return { kind: "text", text: "ok" }; });
  await relay.ready; socket.receive(14, 1, { deviceId: "android:ABC", request: { op: "stalled" } }); await start;
  socket.receive(16, 1, null);
  socket.receive(14, 2, { deviceId: "android:ABC", request: { op: "next" } }); await relay.chain;
  assert.deepEqual(calls, ["stalled", "next"]); assert.equal(socket.sent.length, 1); assert.equal(socket.sent[0].requestId, 2); relay.close();
});
test("BrowserClient routes Android to its SDK without Apple pair records and closes the same SDK", async () => {
  let chosen = false, closed = false; const posts = [];
  const sdk = { requestUSBAccess() { chosen = true; return Promise.resolve({ device: {}, hello: { platform: "android", serialNumber: "ABC", vendorId: 123, productId: 456 } }); }, getPairRecord: async () => undefined,
    pairDevice: async () => ({ relay: { close() {} } }), closeDeviceRelayTarget: async () => { closed = true; } };
  const client = new BrowserClient({ sdk: { requestUSBAccess() { throw new Error("Wrong iOS SDK"); } }, androidSdk: sdk, usbEvents: null, fetchImpl: async (url, options) => {
    if (options.method === "POST") posts.push(JSON.parse(options.body));
    return { ok: true, json: async () => ({ id: "android-session", deviceId: "android:ABC", token: "secret" }) };
  } });
  const pending = client.connect({ apiBase: "http://cloud", platform: "android", accessToken: "controller" }); assert.equal(chosen, true);
  await pending; assert.equal(posts[0].device.platform, "android"); assert.equal(posts[0].device.serialNumber, "android:ABC"); assert.equal(posts[0].pairRecord, undefined);
  assert.equal(client.snapshot().session.platform, "android"); assert.equal(client.snapshot().phase, "connected");
  await client.disconnect(); assert.equal(closed, true);
});

test("ADB shell preserves raw screenshot bytes, reports nonzero exits, and closes cancelled streams", async () => {
  const { runShell } = await import('../web/android/sdk.mjs');
  const { ReadableStream } = await import('@yume-chan/stream-extra');
  let killed = 0, captured;
  const make = (bytes, code = 0) => ({ subprocess: { shellProtocol: { spawn: async (command, abort) => {
    captured = command;
    return { stdout: new ReadableStream({start(c){c.enqueue(bytes);c.close();}}), stderr: new ReadableStream({start(c){c.close();}}),exited:Promise.resolve(code),kill(){killed++;} };
  } } } });
  const raw = Uint8Array.from([137,80,78,71,13,10,26,10,0,255]);
  assert.deepEqual(await runShell(make(raw),'screencap -p',signal(),100,true),raw);assert.equal(captured,'screencap -p');assert.equal(killed,1);
  await assert.rejects(runShell(make(new Uint8Array(),1),'am start',signal(),100),/failed \(1\)/);assert.equal(killed,2);
  const abort = new AbortController();abort.abort();
  await assert.rejects(runShell(make(raw),'input tap 1 2',abort.signal));assert.equal(killed,2,'cancelled input must not dispatch');
});

test("Unicode adapter produces a real published scrcpy clipboard packet with the exact UTF-8 content", async () => {
  const { ScrcpyOptions3_3_3, ScrcpyControlMessageSerializer } = await import('@yume-chan/scrcpy');
  const serializer = new ScrcpyControlMessageSerializer(new ScrcpyOptions3_3_3({video:false,audio:false,control:true}));
  let bytes;
  const { adapter } = fixture({controller:{async setClipboard(message) {
    const result=serializer.setClipboard(message);
    bytes=Array.isArray(result)?result[0]:result;
  }}});
  const content='你好，Android 👋';
  await adapter.execute({op:'type_text',text:content},signal());
  const view=new DataView(bytes.buffer,bytes.byteOffset,bytes.byteLength);
  assert.equal(view.getBigUint64(1),1n);assert.equal(bytes[9],1,'paste must be enabled');
  assert.equal(view.getUint32(10),new TextEncoder().encode(content).length);
  assert.equal(new TextDecoder().decode(bytes.subarray(14)),content);
});

test("parallel Android observation reports tree time independently of slower image capture", async () => {
  let ticks = 0, release;
  const imageReady = new Promise(resolve => { release = resolve; });
  const original = fixture().adapter.capture;
  const { adapter } = fixture({ now: () => ticks, shell: async () => { ticks = 5; return xml; }, capture: async (...args) => { await imageReady; ticks = 80; return original(...args); } });
  const observed = adapter.execute({ op: "observe" }, signal());
  await turn(); release();
  const result = JSON.parse((await observed).text);
  assert.equal(result.timings.elementsMs, 5);
  assert.equal(result.timings.screenshotMs, 80);
});

test("Android element context omits empty wrappers without losing unlabeled focusable controls", () => {
  const { adapter } = fixture();
  const nodes = parseHierarchy('<hierarchy><node class="android.widget.FrameLayout" bounds="[0,0][200,400]"/><node class="android.widget.EditText" focusable="true" bounds="[10,20][100,40]"/><node text="设置" bounds="[10,60][100,100]"/></hierarchy>');
  const result = adapter.formatNodes(nodes).split('\n').map(JSON.parse);
  assert.equal(result.length,2); assert.equal(result[0].type,'android.widget.EditText'); assert.equal(result[1].text,'设置');
  assert.ok(result.every(n => !('index' in n)), 'global tree row must not be confused with a text-match index');
});

test("production control-only options support Unicode paste ACK without subscribing to phone clipboard changes", async () => {
  const { createAndroidScrcpyOptions } = await import('../web/android/options.mjs');
  const { ScrcpyControlMessageSerializer } = await import('@yume-chan/scrcpy');
  const { BufferedReadableStream, ReadableStream } = await import('@yume-chan/stream-extra');
  const options = createAndroidScrcpyOptions();
  assert.equal(options.value.clipboardAutosync,false);
  assert.equal(options.value.sendDeviceMeta,false);
  assert.ok(options.serialize().includes("send_device_meta=false"));
  assert.equal(options.clipboard,undefined);
  await assert.rejects(options.deviceMessageParsers.parse(0,{}),/Unsolicited Android clipboard data rejected/);
  const serializer = new ScrcpyControlMessageSerializer(options);
  const [packet, acknowledged] = serializer.setClipboard({sequence:7n,content:'平板中文输入 🟢',paste:true});
  assert.equal(new TextDecoder().decode(packet.subarray(14)),'平板中文输入 🟢');
  const ack = new Uint8Array(8);new DataView(ack.buffer).setBigUint64(0,7n);
  const stream = new BufferedReadableStream(new ReadableStream({start(c){c.enqueue(ack);c.close();}}));
  await options.deviceMessageParsers.parse(1,stream);
  await acknowledged;
  const pending=serializer.setClipboard({sequence:8n,content:'cancelled',paste:true})[1];
  const rejected=assert.rejects(pending);
  options.deviceMessageParsers.close();
  await rejected;
});

test("published scrcpy client routes real clipboard control packets and ACKs with production options", async () => {
  const { createAndroidScrcpyOptions } = await import('../web/android/options.mjs');
  const { AdbScrcpyClient } = await import('@yume-chan/adb-scrcpy');
  const { Consumable, ReadableStream } = await import('@yume-chan/stream-extra');
  let input,wire;
  const client = new AdbScrcpyClient({ options:createAndroidScrcpyOptions(),
    process:{kill:async()=>input.close(),exited:new Promise(()=>{})},
    output:new ReadableStream({start(c){c.close();}}),
    controlStream:{ readable:new ReadableStream({start(c){input=c;}}),writable:new Consumable.WritableStream({write(bytes){
      wire=bytes;
      const ack=new Uint8Array(9);ack[0]=1;ack.set(bytes.subarray(1,9),1);input.enqueue(ack);
    }})},
  });
  await client.controller.setClipboard({sequence:23n,content:'真实控制通道中文',paste:true});
  assert.equal(new TextDecoder().decode(wire.subarray(14)),'真实控制通道中文');
  await client.close();
});

test("live video observations wait for a new decoded device frame and discard cached frames on failure", async () => {
  const { FrameMailbox } = await import('../web/android/video.mjs');
  const mailbox = new FrameMailbox();mailbox.configure();mailbox.publish({width:1280,height:848});
  let settled=false;
  const fresh=mailbox.wait({after:mailbox.sequence,epoch:mailbox.epoch,timeoutMs:100}).then(frame=>{settled=true;return frame;});
  await turn();assert.equal(settled,false,'a retained frame is not a fresh observation');
  mailbox.publish({width:1280,height:848});assert.equal((await fresh).sequence,2);
  mailbox.fail(new Error('decoder failed'));
  await assert.rejects(mailbox.wait({after:0}),/decoder failed/);assert.equal(mailbox.latest,null);assert.equal(mailbox.waiters.size,0);
});

test("rotation capture waits for a new encoder epoch and two rendered frames, including 180 degrees", async () => {
  const { FrameMailbox, sameViewport } = await import('../web/android/video.mjs');
  const mailbox=new FrameMailbox();mailbox.configure();mailbox.publish({width:1280,height:848});
  let settled=false;
  const fresh=mailbox.wait({after:mailbox.sequence,epoch:mailbox.epoch+1,minFrames:2,timeoutMs:100}).then(frame=>{settled=true;return frame;});
  mailbox.publish({width:1280,height:848});await turn();assert.equal(settled,false);
  mailbox.configure();mailbox.publish({width:1280,height:848});await turn();assert.equal(settled,false);
  mailbox.publish({width:1280,height:848});await fresh;
  assert.equal(sameViewport({width:3048,height:2032,orientation:0},{width:3048,height:2032,orientation:2}),false);
  assert.equal(sameViewport(null,{width:1,height:1,orientation:0}),false);
});

test("missing live frames and cancelled captures retire their waiters rather than returning placeholders", async () => {
  const { FrameMailbox } = await import('../web/android/video.mjs');
  const mailbox=new FrameMailbox();
  await assert.rejects(mailbox.wait({timeoutMs:5}),/No fresh Android video frame/);
  assert.equal(mailbox.waiters.size,0);
  const abort=new AbortController();const result=mailbox.wait({signal:abort.signal});abort.abort();
  await assert.rejects(result);assert.equal(mailbox.waiters.size,0);
});

test("video capture options limit device-side size and rate while keeping audio and clipboard autosync disabled", async () => {
  const { createAndroidScrcpyOptions } = await import('../web/android/options.mjs');
  const options=createAndroidScrcpyOptions({video:true});
  assert.equal(options.value.video,true);assert.equal(options.value.maxSize,1280);assert.equal(options.value.maxFps,10);
  assert.equal(options.value.audio,false);assert.equal(options.value.clipboardAutosync,false);assert.equal(options.value.sendDeviceMeta,false);
  assert.ok(options.serialize().includes('max_size=1280'));
});

test("Android hardware buttons use the persistent control channel and release a key if cancelled after DOWN", async () => {
  const keys=[];const { adapter,calls }=fixture({controller:{async injectKeyCode(key){keys.push(key);}}});
  await adapter.execute({op:'press_button',button:'home'},signal());
  assert.deepEqual(keys.map(key=>[key.action,key.keyCode]),[[0,3],[1,3]]);
  assert.equal(calls.length,0,'no Java shell process for a scrcpy-backed hardware key');
  const cancel=new AbortController();keys.length=0;
  adapter.controller.injectKeyCode=async key=>{keys.push(key);if(key.action===0)cancel.abort();};
  await assert.rejects(adapter.execute({op:'press_button',button:'back'},cancel.signal));
  assert.deepEqual(keys.map(key=>[key.action,key.keyCode]),[[0,4],[1,4]]);
});

test('failed video canvas draws never become fresh device observations', async (t) => {
  const wasm = (await import('../web/wasm.mjs')).default;
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'OffscreenCanvas');
  Object.defineProperty(globalThis, 'OffscreenCanvas', {
    configurable: true,
    value: class {
      constructor(width, height) { this.width = width; this.height = height; }
      getContext() { return {drawImage() { throw new Error('closed decoder frame'); }}; }
    },
  });
  t.after(() => {
    if (previous) Object.defineProperty(globalThis, 'OffscreenCanvas', previous);
    else delete globalThis.OffscreenCanvas;
  });
  let renderer;
  const capture = new wasm.AndroidVideoCapture({
    signal: signal(),
    controller: {},
    readViewport: async () => ({width: 100, height: 200, orientation: 0}),
    video: {metadata: {codec: 0}, stream: new ReadableStream()},
    decoderFactory(options) {
      renderer = options.renderer;
      return {writable: new WritableStream(), dispose() {}};
    },
    streamFactory: sink => new WritableStream(sink),
  });
  t.after(() => capture.close());
  renderer.draw({});
  assert.equal(capture.mailbox.sequence, 0);
  assert.equal(capture.mailbox.latest, null);
  assert.match(capture.mailbox.error.message, /closed decoder frame/);
  await assert.rejects(capture.ready(signal()), /closed decoder frame/);
});

test('Android URL schemes are case insensitive while preserving the original shell argument', async () => {
  const commands = [];
  const {adapter} = fixture({shell: async command => { commands.push(command); return 'Status: ok'; }});
  await adapter.execute({op: 'open_url', url: 'HTTPS://Example.com/CaseSensitive?Key=Value'}, signal());
  assert.equal(commands[0], "am start -W -a android.intent.action.VIEW -d 'HTTPS://Example.com/CaseSensitive?Key=Value'");
  await adapter.execute({op: 'open_url', url: 'MAILTO:Test@Example.com'}, signal());
  assert.equal(commands[1], "am start -W -a android.intent.action.VIEW -d 'MAILTO:Test@Example.com'");
  await assert.rejects(adapter.execute({op: 'open_url', url: 'JAVASCRIPT:alert(1)'}, signal()), /Unsupported URL scheme/);
  assert.equal(commands.length, 2);
});
