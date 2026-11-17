import { Adb, AdbDaemonTransport } from "@yume-chan/adb";
import AdbWebCredentialStore from "@yume-chan/adb-credential-web";
import { AdbScrcpyClient } from "@yume-chan/adb-scrcpy";
import { ReadableStream } from "@yume-chan/stream-extra";
import { AndroidCommands, inputViewport } from "./core.mjs";
import { AndroidVideoCapture } from "./video.mjs";
import { createAndroidScrcpyOptions } from "./options.mjs";
import { AndroidRelay, encodeFrame } from "./relay.mjs";

const SERVER_HASH = "7e70323ba7f259649dd4acce97ac4fefbae8102b2c6d91e2e7be613fd5354be0";
const SERVER_PATH = "/data/local/tmp/ictl-scrcpy-3.3.3.jar";
// Browser SDK construction remains here; shell/capture behavior is Rust/WASM.
import wasm from "../wasm.mjs";
export const { readBytes, runShell } = wasm;
const capture = wasm.captureAndroid;
function ensureOpen(target) { if (target.closed) throw new Error("Android connection cancelled"); }
export async function pairDevice({ registryApiUrl, token, target, log = () => {} }) {
  try {
    ensureOpen(target); log("Claiming usbmux interface");
    const connection = await target.selected.connect();
    ensureOpen(target); log("Claimed usbmux interface");
    log("Android authorization");
    const transport = await AdbDaemonTransport.authenticate({ serial: target.selected.serial, connection, credentialStore: new AdbWebCredentialStore("Phone Control") });
    target.adb = new Adb(transport);
    ensureOpen(target);
    const signal = target.abort.signal;
    const version = await target.adb.getProp("ro.build.version.sdk");
    ensureOpen(target);
    if (Number(version) < 26 || !Number.isFinite(Number(version))) throw new Error("当前 Android 控制需要 Android 8 或更高版本");
    const response = await fetch(new URL("./vendor/scrcpy-server-v3.3.3.bin", import.meta.url), { signal });
    if (!response.ok) throw new Error("无法读取 Android 控制组件");
    const server = new Uint8Array(await response.arrayBuffer());
    const hash = [...new Uint8Array(await crypto.subtle.digest("SHA-256", server))].map((n) => n.toString(16).padStart(2, "0")).join("");
    if (hash !== SERVER_HASH) throw new Error("Android 控制组件校验失败");
    ensureOpen(target);
    log("Android control startup");
    for (const video of AndroidVideoCapture.isSupported ? [true, false] : [false]) {
      ensureOpen(target);
      const options = createAndroidScrcpyOptions({ video });
      const path = SERVER_PATH.replace(".jar", `-${options.value.scid}.jar`);
      try {
        await AdbScrcpyClient.pushServer(target.adb, new ReadableStream({ start(controller) { controller.enqueue(server); controller.close(); } }), path);
        ensureOpen(target);
        target.scrcpy = await AdbScrcpyClient.start(target.adb, path, options);
        ensureOpen(target);
        if (!target.scrcpy.controller) throw new Error("Android 输入控制通道不可用");
        void target.scrcpy.output.pipeTo(new WritableStream({ write() {} })).catch(() => {});
        if (video) {
          const stream = await target.scrcpy.videoStream;
          target.video = new AndroidVideoCapture({ video: stream, controller: target.scrcpy.controller,
            readViewport: async abort => inputViewport(await runShell(target.adb, "dumpsys input", abort, 3000)), signal });
          await target.video.ready(signal);
          log("Android video ready");
        }
        break;
      } catch (error) {
        target.video?.close(); target.video = null;
        await target.scrcpy?.close(); target.scrcpy = null;
        ensureOpen(target);
        if (!video) throw error;
        console.warn("Android hardware capture fallback:", String(error?.message || "unavailable").slice(0, 240));
        log("Android video fallback");
      }
    }
    const commands = new AndroidCommands({ deviceId: target.hello.serialNumber, shell: (command, abort, timeout) => runShell(target.adb, command, abort, timeout), capture: (abort, region, previous) => capture(target.adb, abort, region, previous, target.video), controller: target.scrcpy.controller });
    const url = new URL("/device/ws", registryApiUrl);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    // The session token travels as a WebSocket subprotocol, not in the URL, so it
    // cannot end up in proxy or access logs. The gateway selects `ictl.relay`.
    const socket = new WebSocket(url, ["ictl.relay", `ictl.token.${token}`]);
    const relay = new AndroidRelay(socket, target.hello.serialNumber, (request, abort) => commands.execute(request, abort), { log, onClose: () => void closeDeviceRelayTarget(target) });
    target.relay = relay;
    socket.addEventListener("open", () => {
      if (target.closed) { relay.close(); return; }
      socket.send(encodeFrame(1, 0, target.hello)); log("Connected WebSocket relay");
    }, { once: true });
    void target.adb.disconnected.then(() => relay.close(), () => relay.close());
    void target.scrcpy.exited.then(() => relay.close(), () => relay.close());
    await relay.ready;
    ensureOpen(target);
    log("Android ready");
    return { relay };
  } catch (error) {
    await closeDeviceRelayTarget(target);
    throw error;
  }
}
export async function closeDeviceRelayTarget(target) {
  target.closed = true;
  target.abort.abort(); target.relay?.close(); target.video?.close();
  await Promise.allSettled([target.device?.close(), target.adb?.close(), target.scrcpy?.close()]);
}
