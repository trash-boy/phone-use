import init, { mount } from "/pkg/ictl_web.js";
await init();
const UID = "00008140-001234567890ABCD";
const canvas = document.createElement("canvas");
canvas.width = 100;
canvas.height = 200;
canvas.getContext("2d").fillRect(0, 0, 100, 200);
const fixture = (window.fixture = {
  calls: [],
  ai: {
    aiReady: true,
    model: "fixture/default",
    models: ["fixture/default", "fixture/other"],
  },
  state: {
    id: "session",
    deviceId: UID,
    state: "connected",
    ready: true,
    daemon: { state: { state: "ready" } },
    setup: {
      state: "complete",
      transcript: "Fixture setup output",
      exitCode: 0,
    },
    task: null,
  },
  image: {
    deviceId: UID,
    imageBase64: canvas.toDataURL("image/jpeg").split(",")[1],
    caption: "Explicit fixture",
    timings: { screenshotMs: 123 },
    elementsError: "Fixture tree unavailable",
  },
});
let releaseConfirm, releasePair, releasePreview, releaseCommand;
fixture.releaseConfirm = () => releaseConfirm?.();
fixture.releasePair = () => releasePair?.();
fixture.releasePreview = () => releasePreview?.();
fixture.releaseCommand = () => releaseCommand?.();
const sdk = {
  requestUSBAccess() {
    fixture.calls.push({
      choose: true,
      active: navigator.userActivation.isActive,
    });
    return Promise.resolve({
      hello: {
        serialNumber: UID,
        vendorId: 1452,
        productId: 4776,
        productName: "Explicit browser fixture",
      },
      device: {},
    });
  },
  getPairRecord: async () => undefined,
  putPairRecord: async () => {},
  async pairDevice() {
    fixture.pairStarted = true;
    if (fixture.holdPair)
      await new Promise((resolve) => (releasePair = resolve));
    return { relay: { close() {} } };
  },
  closeDeviceRelayTarget: async () => {},
};
const fetchImpl = async (url, options = {}) => {
  const body = options.body ? JSON.parse(options.body) : null;
  fixture.calls.push({ url, method: options.method, body });
  const reply = (data) =>
    new Response(JSON.stringify(data), {
      headers: { "Content-Type": "application/json" },
    });
  if (url.endsWith("/api/config")) return reply(fixture.ai);
  if (url.endsWith("/api/sessions") && options.method === "POST") {
    fixture.sessionCount = (fixture.sessionCount || 0) + 1;
    fixture.state.id = `session-${fixture.sessionCount}`;
    fixture.state.platform = body.device.platform || "ios";
    fixture.state.deviceId = body.device.serialNumber;
    fixture.image.deviceId = body.device.serialNumber;
    return reply({
      id: fixture.state.id,
      deviceId: body.device.serialNumber,
      token: "explicit-fixture-token",
    });
  }
  if (url.endsWith("/preview/stream") && fixture.enableStream) {
    return reply({ url: `/api/sessions/${fixture.state.id}/preview/mjpeg/browser-fixture` });
  }
  if (url.endsWith("/attachments") && options.method === "POST") {
    return reply({ id: `image-${fixture.calls.length}`, name: body.name, mimeType: body.mimeType, size: atob(body.dataBase64).length });
  }
  // Older gateways have no screen.jpg route: the default reply below is JSON,
  // which the client must treat as unsupported and fall back to imageBase64.
  if (url.endsWith("/screen.jpg") && fixture.serveScreenJpeg) {
    const bytes = Uint8Array.from(atob(fixture.image.imageBase64), (c) => c.charCodeAt(0));
    return new Response(bytes, { headers: { "Content-Type": "image/jpeg", "Cache-Control": "no-store" } });
  }
  if (url.endsWith("/preview")) {
    if (fixture.holdNextPreview) {
      fixture.holdNextPreview = false;
      const stale = document.createElement("canvas");
      stale.width = 100;
      stale.height = 200;
      stale.getContext("2d").fillStyle = "#f00";
      stale.getContext("2d").fillRect(0, 0, 100, 200);
      fixture.staleSource = stale.toDataURL("image/jpeg");
      const held = {
        ...fixture.image,
        imageBase64: fixture.staleSource.split(",")[1],
      };
      fixture.heldPreview = url;
      return new Promise(
        (resolve) => (releasePreview = () => resolve(reply(held))),
      );
    }
    if (fixture.freezeImage) return reply(fixture.image);
    fixture.frames = (fixture.frames || 0) + 1;
    canvas.getContext("2d").fillStyle = fixture.frames % 2 ? "#234" : "#567";
    canvas.getContext("2d").fillRect(0, 0, 100, 200);
    fixture.image.imageBase64 = canvas.toDataURL("image/jpeg").split(",")[1];
    return reply(fixture.image);
  }
  if (url.endsWith("/command")) {
    if (fixture.holdNextCommand) {
      fixture.holdNextCommand = false;
      fixture.heldCommand = url;
      return new Promise(
        (resolve) =>
          (releaseCommand = () => resolve(reply({ kind: "text", text: "{}" }))),
      );
    }
    return reply({ kind: "text", text: "{}" });
  }
  if (url.endsWith("/setup/input")) return reply({});
  if (url.endsWith("/setup")) {
    fixture.state.setup.state = "running";
    return reply(fixture.state.setup);
  }
  if (url.endsWith("/tasks")) {
    fixture.state.task = {
      id: "task",
      instruction: body.instruction,
      model: body.model,
      state: "awaiting_confirmation",
      steps: [
        {
          id: "step",
          tool: "iphone_type_text",
          reason: "Explicit fixture confirmation",
          state: "awaiting_confirmation",
        },
      ],
      timings: { observationMs: 100, modelMs: 200, actionMs: 300 },
      confirmation: {
        actionId: "action",
        tool: "iphone_type_text",
        args: { text: "消息", submit: true },
        reason: "发送前确认",
      },
    };
    return reply(fixture.state.task);
  }
  if (url.endsWith("/instruction")) {
    const task = fixture.state.task;
    fixture.state.task = {
      ...task,
      instruction: body.instruction,
      instructionUpdates: [
        ...(task.instructionUpdates || []),
        { instruction: body.instruction, previous: task.instruction, afterStep: task.steps.length },
      ],
    };
    return reply(fixture.state.task);
  }
  if (url.endsWith("/confirm")) {
    if (fixture.holdConfirm)
      await new Promise((resolve) => (releaseConfirm = resolve));
    return reply(fixture.state.task);
  }
  if (url.endsWith("/stop")) {
    fixture.state.task = {
      ...fixture.state.task,
      state: "stopped",
      confirmation: null,
    };
    return reply(fixture.state.task);
  }
  if (options.method === "DELETE") return reply({});
  return reply(fixture.state);
};
mount({
  sdk,
  androidSdk: {
    ...sdk,
    requestUSBAccess() {
      fixture.calls.push({
        choose: true,
        active: navigator.userActivation.isActive,
      });
      return Promise.resolve({
        hello: {
          serialNumber: "fixture-android",
          vendorId: 1256,
          productId: 1,
          productName: "Explicit Android fixture",
        },
        device: {},
      });
    },
  },
  fetchImpl,
});
