import test, { before, after } from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { chromium } from "playwright";
import { createServer, preview } from "vite";
import { toPhonePoint, pointerCommand } from "../web/interaction.mjs";

let browser, dev, built, base, builtBase;
let streamFixtureBytes;
const root = new URL("..", import.meta.url).pathname;
const fixturePage = `<!doctype html>
<html lang="zh-CN">
  <head><link rel="stylesheet" href="/web/styles.css"></head>
  <body>
    <div id="root"></div>
    <script type="module" src="/__test__/fixture.mjs"></script>
  </body>
</html>`;
const fixtureScript = await readFile(
  new URL("./fixtures/browser-console.mjs", import.meta.url),
  "utf8",
);

before(async () => {
  const chrome =
    process.env.CHROME_BIN ||
    (process.platform === "darwin"
      ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      : undefined);
  browser = await chromium.launch({
    headless: true,
    ...(chrome && existsSync(chrome) ? { executablePath: chrome } : {}),
  });
  dev = await createServer({
    root,
    configFile: new URL("../vite.config.mjs", import.meta.url).pathname,
    server: { host: "127.0.0.1", port: 0, strictPort: false },
    plugins: [
      {
        name: "explicit-browser-fixture",
        configureServer(server) {
          server.middlewares.use((req, res, next) => {
            if (req.url?.endsWith('/preview/mjpeg/browser-fixture') && streamFixtureBytes) {
              res.writeHead(200, { 'Content-Type': 'multipart/x-mixed-replace; boundary=fixture', 'Cache-Control': 'no-store' });
              // Keep this response's frame: a finished test clears streamFixtureBytes while the timer may still fire.
              const frame = streamFixtureBytes;
              const write = () => res.write(Buffer.concat([Buffer.from(`--fixture\r\nContent-Type: image/jpeg\r\nContent-Length: ${frame.length}\r\n\r\n`), frame, Buffer.from('\r\n')]));
              write(); const timer = setInterval(write, 100);
              res.on('close', () => clearInterval(timer));
              return;
            }
            if (req.url === "/__test__/fixture.mjs") {
              res.setHeader("Content-Type", "text/javascript");
              res.end(fixtureScript);
              return;
            }
            if (req.url !== "/__test__/console.html") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(fixturePage);
          });
        },
      },
    ],
  });
  await dev.listen();
  base = `http://127.0.0.1:${dev.httpServer.address().port}`;
  built = await preview({
    root,
    configFile: new URL("../vite.config.mjs", import.meta.url).pathname,
    preview: { host: "127.0.0.1", port: 0, strictPort: false },
  });
  builtBase = `http://127.0.0.1:${built.httpServer.address().port}`;
});
after(async () => {
  await browser?.close();
  await dev?.close();
  if (built) await new Promise((resolve) => built.httpServer.close(resolve));
});
async function pageFor(t, fixture = true) {
  const page = await browser.newPage({
    viewport: { width: 1440, height: 1100 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  t.after(async () => {
    await page.close();
    assert.deepEqual(
      errors,
      [],
      "browser must have no uncaught runtime errors",
    );
  });
  if (!fixture)
    await page.route("**/api/config", (route) =>
      route.fulfill({ json: { aiReady: false, model: null, models: [] } }),
    );
  await page.goto(fixture ? base + "/__test__/console.html" : builtBase + "/");
  await page.getByRole("heading", { name: "设备工作台", exact: true }).waitFor();
  return page;
}
async function openConnection(page) {
  if (!await page.locator("#server-token").isVisible()) {
    await page.getByRole("button", { name: /^(连接设备|连接设置)$/ }).click();
  }
  await page.locator("#server-token").waitFor();
}
async function closeConnection(page) {
  await page.getByRole("button", { name: "关闭连接设置", exact: true }).click();
}
async function connect(page, platform = "ios", capture = true) {
  await openConnection(page);
  await page.locator("#device-platform").selectOption(platform);
  await page.locator("#server-token").fill("explicit-fixture-controller");
  await page
    .getByRole("button", {
      name: platform === "android" ? "连接 Android" : "连接 iPhone",
      exact: true,
    })
    .click();
  await page.getByRole("button", { name: "断开连接", exact: true }).waitFor();
  await page.waitForFunction(() =>
    document
      .querySelector(".connection-status")
      .textContent.includes("可以控制"),
  );
  await closeConnection(page);
  if (capture && platform === "ios") {
    await page.getByRole('button', {name: '刷新屏幕', exact: true}).click();
    await page.waitForFunction(() => document.querySelector('.phone-screen')?.naturalWidth > 0);
  }
}

test("production bootstrap loads Rust WASM UI with honest empty phone and accessible offline controls", async (t) => {
  const page = await pageFor(t, false);
  await page.getByText("AI 尚未配置", { exact: true }).waitFor();
  assert.equal(await page.locator(".phone-screen").count(), 0);
  assert.equal(
    await page
      .getByRole("button", { name: "主屏幕", exact: true })
      .isDisabled(),
    true,
  );
  assert.equal(await page.locator("#task-model").isDisabled(), true);
  await page.getByRole("button", { name: "回到主屏幕", exact: false }).click();
  assert.equal(
    await page.locator("#task-instruction").inputValue(),
    "回到主屏幕",
  );
  await page.locator("#task-instruction").fill("中文😀");
  assert.match(
    await page.locator(".task-input-footer").textContent(),
    /4\/4000/,
  );
  await page.locator("#task-instruction").focus();
  await page.getByRole("button", { name: "刷新列表", exact: true }).click();
  await page.locator("#task-instruction").focus();
  await page.getByText("AI 尚未配置", { exact: true }).waitFor();
  assert.equal(
    await page
      .locator("#task-instruction")
      .evaluate((node) => node === document.activeElement),
    true,
  );
  assert.equal(await page.locator("#device-platform option").count(), 2);
});

test("composer and execution history are distinct named regions with a keyboard skip link", async (t) => {
  const page = await pageFor(t);
  const composer = page.getByRole("region", { name: "告诉手机，你想做什么", exact: true });
  const history = page.getByRole("region", { name: "执行记录", exact: true });
  assert.equal(await composer.count(), 1);
  assert.equal(await history.count(), 1);
  assert.equal(await composer.getByLabel("任务描述", { exact: true }).count(), 1);
  assert.equal(await composer.getByRole("region", { name: "执行记录", exact: true }).count(), 0);
  await history.getByText("等待第一个任务", { exact: true }).waitFor();
  await page.keyboard.press("Tab");
  const skip = page.getByRole("link", { name: "跳到工作台", exact: true });
  assert.equal(await skip.evaluate(node => node === document.activeElement), true);
  await page.keyboard.press("Enter");
  assert.equal(await page.locator("main").evaluate(node => node === document.activeElement), true);
});

test("connection settings open on demand, contain keyboard focus, and close back to their trigger", async (t) => {
  const page = await pageFor(t);
  assert.equal(await page.locator("#server-token").isVisible(), false);
  const trigger = page.getByRole("button", { name: "连接设备", exact: true });
  await trigger.click();
  const dialog = page.getByRole("dialog", { name: "连接设备", exact: true });
  await dialog.waitFor();
  assert.equal(await dialog.evaluate(node => node.contains(document.activeElement)), true);
  await page.locator("#device-platform").selectOption("android");
  await page.keyboard.press("Escape");
  assert.equal(await page.locator("#server-token").isVisible(), false);
  assert.equal(await trigger.evaluate(node => node === document.activeElement), true);
  await trigger.click();
  assert.equal(await page.locator("#device-platform").inputValue(), "android");
  await page.getByRole("button", { name: "关闭连接设置", exact: true }).click();
  assert.equal(await trigger.evaluate(node => node === document.activeElement), true);
});

test("desktop agent workspace shows execution above a composer reachable in the first viewport", async (t) => {
  const page = await pageFor(t);
  await page.setViewportSize({ width: 1440, height: 900 });
  const history = await page.getByRole("region", { name: "执行记录", exact: true }).boundingBox();
  const composer = await page.getByRole("region", { name: "告诉手机，你想做什么", exact: true }).boundingBox();
  assert.ok(history.y < composer.y, "execution should precede the docked task input");
  assert.ok(composer.y + composer.height <= 900, "task input should fit the first desktop viewport");
});

test("a full composer cannot cover Stop when attachments, options and enlarged text fill the workspace", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  const png = Buffer.from(await page.evaluate(() => {
    const canvas = document.createElement("canvas"); canvas.width = 16; canvas.height = 16;
    return canvas.toDataURL("image/png").split(",")[1];
  }), "base64");
  await page.getByLabel("添加任务图片").setInputFiles(Array.from({ length: 6 }, (_, i) => ({
    name: `reference-${i}.png`, mimeType: "image/png", buffer: png,
  })));
  await page.waitForFunction(() => document.querySelectorAll(".image-thumbnails img").length === 6);
  await page.locator(".task-options summary").click();
  await page.locator("#task-instruction").fill("查看设备");
  await page.locator("#task-instruction").evaluate(node => node.style.height = "260px");
  await page.waitForFunction(() => !document.querySelector(".task-input-footer button").disabled);
  await page.locator(".task-composer").evaluate(form => form.requestSubmit());
  await page.getByText("这一步需要你的确认", { exact: true }).waitFor();
  for (const state of [
    { width: 932, height: 430, font: "14px" },
    { width: 1440, height: 900, font: "28px" },
  ]) {
    await page.setViewportSize({ width: state.width, height: state.height });
    await page.evaluate(font => { document.documentElement.style.fontSize = font; window.scrollTo(0, 0); }, state.font);
    const stop = page.getByRole("button", { name: "停止任务", exact: true });
    assert.equal(await stop.evaluate(node => {
      const r = node.getBoundingClientRect();
      return document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2)?.closest("button") === node;
    }), true, "Stop must remain visible and receive pointer events");
  }
  await page.getByRole("button", { name: "停止任务", exact: true }).click();
  await page.getByText("任务已停止", { exact: false }).waitFor();
});

test("empty device guidance is not clipped by a phone frame at enlarged text sizes", async (t) => {
  const page = await pageFor(t);
  await page.evaluate(() => document.documentElement.style.fontSize = "28px");
  for (const viewport of [{ width: 932, height: 430 }, { width: 1440, height: 900 }]) {
    await page.setViewportSize(viewport);
    assert.equal(await page.locator(".empty-phone").evaluate(node => {
      const text = node.getBoundingClientRect();
      const frame = node.closest(".phone-frame").getBoundingClientRect();
      return text.top >= frame.top && text.bottom <= frame.bottom;
    }), true, "empty-state text must fit its container");
  }
});

test("mobile workspace keeps controls reachable and long confirmation content inside the viewport", async (t) => {
  const page = await pageFor(t);
  await page.setViewportSize({ width: 375, height: 812 });
  await connect(page, "android");
  await page.locator("#task-instruction").fill("检查设备");
  await page.getByRole("button", { name: "开始任务", exact: false }).click();
  await page.getByText("这一步需要你的确认", { exact: true }).waitFor();
  await page.evaluate(() => {
    fixture.state.task.confirmation.args.text = "https://example.test/" + "long-token".repeat(60);
    fixture.state.task.steps[0].reason = "检查目标：" + "long-token".repeat(60);
  });
  await page.waitForFunction(() => document.querySelector(".confirmation pre").textContent.includes("long-token"));
  for (const viewport of [{ width: 375, height: 812 }, { width: 812, height: 375 }, { width: 932, height: 430 }]) {
    await page.setViewportSize(viewport);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true);
    for (const label of ["停止任务", "确认这一步", "拒绝并停止", "刷新屏幕"]) {
      const box = await page.getByRole("button", { name: label, exact: true }).boundingBox();
      assert.ok(box.width >= 44 && box.height >= 44, `${label} must have a comfortable touch target`);
    }
  }
  await page.getByRole("button", { name: "停止任务", exact: true }).click();
  await page.getByText("任务已停止", { exact: false }).waitFor();
});

test("Android preview guidance only advertises available controls", async (t) => {
  const page = await pageFor(t);
  await connect(page, "android");
  assert.equal(await page.getByRole("button", { name: "实时画面", exact: true }).count(), 0);
  assert.doesNotMatch(await page.locator(".phone-card").textContent(), /实时画面/);
});

test("Rust gestures preserve letterbox mapping and pointer gesture bounds", () => {
  const rect = { left: 20, top: 10, width: 200, height: 200 };
  assert.deepEqual(toPhonePoint(120, 110, rect, 100, 200), { x: 50, y: 100 });
  assert.equal(toPhonePoint(25, 110, rect, 100, 200), null);
  const start = { point: { x: 1, y: 2 }, x: 0, y: 0, time: 0 };
  assert.equal(
    pointerCommand(start, { point: { x: 1, y: 2 }, x: 0, y: 0, time: 20000 })
      .args.seconds,
    10,
  );
  assert.equal(
    pointerCommand(start, { point: { x: 4, y: 5 }, x: 20, y: 20, time: 300 })
      .tool,
    "iphone_swipe",
  );
});

test("iPhone readiness gate, setup labels and prompt input retain current behavior", async (t) => {
  const page = await pageFor(t);
  await page.evaluate(() => (fixture.state.daemon.state.state = "needs_trust"));
  await openConnection(page);
  await page.locator("#server-token").fill("explicit-fixture-controller");
  await page.getByRole("button", { name: "连接 iPhone", exact: true }).click();
  await page.getByRole("button", { name: "断开连接", exact: true }).waitFor();
  await closeConnection(page);
  await page.getByText("WDA 准备中", { exact: true }).first().waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "主屏幕", exact: true })
      .isDisabled(),
    true,
  );
  await page.getByText("向导已结束", { exact: true }).waitFor();
  await page.getByRole("button", { name: "重新启动向导", exact: true }).click();
  await page.locator("#setup-line").fill("explicit-fake-2fa");
  await page.getByRole("button", { name: "提交", exact: true }).click();
  assert.equal(await page.locator("#setup-line").inputValue(), "");
  await page.evaluate(() => (fixture.state.daemon.state.state = "ready"));
  await page.waitForFunction(
    () => !document.querySelector(".hardware-buttons button").disabled,
  );
  const choices = await page.evaluate(() =>
    fixture.calls.filter((c) => c.choose),
  );
  assert.equal(choices.length, 1);
  assert.equal(choices[0].active, true);
  assert.equal(await page.locator("#server-token").inputValue(), "");
});

test("Android ready UI provides back control and excludes Apple setup", async (t) => {
  const page = await pageFor(t);
  await connect(page, "android");
  assert.equal(
    await page.getByRole("button", { name: "返回", exact: true }).isEnabled(),
    true,
  );
  assert.equal(await page.locator(".setup-transcript").count(), 0);
  await page.getByRole("button", { name: "返回", exact: true }).click();
  await page.waitForFunction(() =>
    fixture.calls.some((c) => c.body?.args?.button === "back"),
  );
});

test("recovering task shows the waiting message, uncertain step and usable Stop control", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  await page.evaluate(() => {
    fixture.state.task = {
      id: "task", instruction: "Inspect fixture screen", state: "recovering", confirmation: null,
      message: "正在等待 WDA 恢复，最多 120 秒。",
      steps: [{ id: "step", tool: "iphone_tap", reason: "Inspect fixture target", state: "uncertain" }],
    };
  });
  await page.getByText("正在等待设备恢复", { exact: false }).waitFor();
  await page.getByText("正在等待 WDA 恢复，最多 120 秒。", { exact: true }).waitFor();
  assert.equal(await page.locator(".step-state").textContent(), "结果不确定");
  assert.equal(await page.locator("#task-instruction").isDisabled(), true);
  await page.getByRole("button", { name: "停止任务", exact: true }).click();
  await page.waitForFunction(() => document.querySelector(".task-status")?.textContent.includes("任务已停止"));
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith("/stop")).length), 1);
});

test("running task accepts a goal update and shows the remaining plan", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  await page.evaluate(() => {
    fixture.state.task = {
      id: "task", instruction: "打开设置", state: "executing", confirmation: null, timings: {},
      plan: [{ tool: "iphone_type_text", reason: "输入" }, { tool: "iphone_wait_for", reason: "等待" }],
      steps: [
        { id: "step", tool: "iphone_tap", reason: "点搜索框", state: "succeeded", planned: false },
        { id: "step-2", tool: "iphone_press_button", reason: "回到主屏幕", state: "executing", planned: true },
      ],
    };
  });
  await page.getByText("已规划、待执行：输入文字 → 等待界面", { exact: true }).waitFor();
  assert.match(await page.locator(".step-list li").nth(1).textContent(), /连续执行/);
  assert.doesNotMatch(await page.locator(".step-list li").nth(0).textContent(), /连续执行/);
  await page.locator("#task-goal-update").fill("改为打开时钟");
  await page.getByRole("button", { name: "更新目标", exact: true }).click();
  await page.waitForFunction(() =>
    fixture.calls.some((c) => c.url?.endsWith("/tasks/task/instruction") && c.body?.instruction === "改为打开时钟"),
  );
  await page.getByText("第 2 步后更新了目标，原目标：打开设置", { exact: true }).waitFor();
  assert.equal(await page.locator(".current-instruction").textContent(), "改为打开时钟");
  assert.equal(await page.locator("#task-goal-update").inputValue(), "");
});

test("a failed task offers Continue, which starts a resuming task with the same model and auto-confirm", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  await page.evaluate(() => {
    fixture.state.task = {
      id: "task", instruction: "打开设置并搜索蓝牙", model: "fixture/other", autoConfirm: true, state: "failed",
      error: "连续 6 步操作后屏幕都没有变化", confirmation: null, steps: [],
    };
  });
  await page.getByRole("button", { name: "继续任务", exact: true }).click();
  await page.waitForFunction(() => fixture.calls.some((c) => c.url?.endsWith("/tasks") && c.method === "POST"));
  const body = await page.evaluate(() => fixture.calls.filter((c) => c.url?.endsWith("/tasks")).at(-1).body);
  assert.match(body.instruction, /继续完成上一个任务：打开设置并搜索蓝牙/);
  assert.match(body.instruction, /上次结束原因：连续 6 步操作后屏幕都没有变化/);
  assert.equal(body.model, "fixture/other");
  assert.equal(body.autoConfirm, true);
});

test("model selection, confirmation details and Stop remain independent of held confirm request", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  await page.locator("#task-model").selectOption("fixture/other");
  await page.locator("#task-instruction").fill("发送前确认");
  await page.getByRole("button", { name: "开始任务", exact: false }).click();
  await page.getByText("这一步需要你的确认", { exact: true }).waitFor();
  assert.equal(await page.locator("#task-model").isDisabled(), true);
  assert.equal(
    await page
      .getByRole("button", { name: "主屏幕", exact: true })
      .isDisabled(),
    true,
  );
  assert.match(
    await page.locator(".confirmation pre").textContent(),
    /"submit": true/,
  );
  assert.match(
    await page.locator('[aria-label="最近一轮耗时"]').textContent(),
    /观察 0.10 秒.*模型 0.20 秒.*操作 0.30 秒/,
  );
  assert.match(
    await page
      .locator(".field-hint")
      .allTextContents()
      .then((x) => x.join(" ")),
    /本次任务模型：fixture\/other/,
  );
  await page.evaluate(() => (fixture.holdConfirm = true));
  await page.getByRole("button", { name: "确认这一步", exact: true }).click();
  await page.waitForFunction(() =>
    fixture.calls.some((c) => c.url?.endsWith("/confirm")),
  );
  assert.equal(
    await page
      .getByRole("button", { name: "停止任务", exact: true })
      .isEnabled(),
    true,
  );
  await page.getByRole("button", { name: "停止任务", exact: true }).click();
  await page.waitForFunction(() =>
    document.querySelector(".task-status")?.textContent.includes("任务已停止"),
  );
  assert.equal(
    await page.evaluate(
      () => fixture.calls.filter((c) => c.url?.endsWith("/stop")).length,
    ),
    1,
  );
  await page.evaluate(() => fixture.releaseConfirm());
  await page.waitForTimeout(50);
  assert.match(await page.locator(".task-status").textContent(), /任务已停止/);
});

test("screen refresh preserves image pointer capture and text focus while manual input dispatches once", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  const image = page.locator(".phone-screen");
  await image.waitFor();
  await page.waitForFunction(
    () => document.querySelector(".phone-screen")?.naturalWidth === 100,
  );
  await image.evaluate((node) => (window.capturedImage = node));
  const initialSource = await image.getAttribute("src");
  await page.locator("#phone-text").fill("中文输入");
  await page.locator("#phone-text").focus();
  await page.waitForTimeout(2100);
  assert.equal(
    await image.evaluate((node) => node === window.capturedImage),
    true,
  );
  assert.equal(await image.getAttribute("src"), initialSource, 'idle status polling must not capture');
  assert.equal(
    await page
      .locator("#phone-text")
      .evaluate((node) => node === document.activeElement),
    true,
  );
  await page.getByRole("button", { name: "输入", exact: true }).click();
  await page.waitForFunction(() =>
    fixture.calls.some((c) => c.body?.tool === "iphone_type_text"),
  );
  assert.equal(await page.locator("#phone-text").inputValue(), "");
  assert.equal(
    await page.evaluate(
      () =>
        fixture.calls.filter((c) => c.body?.tool === "iphone_type_text").length,
    ),
    1,
  );
  await page.waitForFunction(
    () => !document.querySelector("#phone-text").disabled,
  );
  await page.waitForFunction(source => document.querySelector('.phone-screen')?.getAttribute('src') !== source, initialSource);
  const box = await image.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  assert.notEqual(
    await page.locator(".screen-meta code").textContent(),
    "坐标随指针显示",
  );
  await page.mouse.down();
  await page.waitForTimeout(2100);
  assert.equal(
    await image.evaluate((node) => node === window.capturedImage),
    true,
  );
  await page.mouse.up();
  await page.waitForFunction(() =>
    fixture.calls.some((c) => c.body?.tool === "iphone_long_press"),
  );
});

test("cancelled pending pairing shows USB custody wait until the SDK settles", async (t) => {
  const page = await pageFor(t);
  await page.evaluate(() => (fixture.holdPair = true));
  await openConnection(page);
  await page.locator("#server-token").fill("explicit-fixture-controller");
  await page.getByRole("button", { name: "连接 iPhone", exact: true }).click();
  await page.waitForFunction(() => fixture.pairStarted);
  await page.getByRole("button", { name: "取消连接", exact: true }).click();
  await page
    .getByRole("button", { name: "等待 USB 释放", exact: true })
    .waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "等待 USB 释放", exact: true })
      .isDisabled(),
    true,
  );
  await page.evaluate(() => fixture.releasePair());
  await page
    .getByRole("button", { name: "连接 iPhone", exact: true })
    .waitFor();
});

test("setup transcript preserves user scroll on status polls and follows actual new output", async (t) => {
  const page = await pageFor(t);
  await page.evaluate(
    () =>
      (fixture.state.setup.transcript = Array.from(
        { length: 100 },
        (_, i) => `line ${i}`,
      ).join("\n")),
  );
  await connect(page);
  const transcript = page.locator(".setup-transcript");
  await page.waitForFunction(() =>
    document
      .querySelector(".setup-transcript")
      ?.textContent.includes("line 99"),
  );
  await transcript.evaluate((node) => (node.scrollTop = 0));
  await page.waitForTimeout(1300);
  assert.equal(
    await transcript.evaluate((node) => node.scrollTop),
    0,
    "unchanged polling must preserve the user's scroll position",
  );
  await page.evaluate(
    () => (fixture.state.setup.transcript += "\nnew fixture output"),
  );
  await page.waitForFunction(() =>
    document
      .querySelector(".setup-transcript")
      ?.textContent.includes("new fixture output"),
  );
  await page.waitForFunction(() => {
    const node = document.querySelector(".setup-transcript");
    return (
      node.scrollTop > 0 &&
      Math.abs(node.scrollHeight - node.clientHeight - node.scrollTop) <= 1
    );
  });
});

test("a manual preview from a previous session cannot replace a reconnected same-device screen", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  await page.locator(".phone-screen").waitFor();
  await page.evaluate(() => (fixture.holdNextPreview = true));
  await page.getByRole("button", { name: "刷新屏幕", exact: true }).click();
  await page.waitForFunction(() => fixture.heldPreview);
  await openConnection(page);
  await page.getByRole("button", { name: "断开连接", exact: true }).click();
  await connect(page);
  await page.locator(".phone-screen").waitFor();
  assert.equal(await page.evaluate(() => fixture.state.id), "session-2");
  await page.evaluate(() => fixture.releasePreview());
  await page.waitForFunction(
    () => !document.querySelector("#phone-text").disabled,
  );
  assert.equal(
    await page
      .locator(".phone-screen")
      .evaluate((node) => node.src === fixture.staleSource),
    false,
  );
});

test("a delayed command from a previous session cannot start a screenshot request in the new session", async (t) => {
  const page = await pageFor(t);
  await connect(page);
  await page.locator(".phone-screen").waitFor();
  await page.evaluate(() => (fixture.holdNextCommand = true));
  await page.getByRole("button", { name: "主屏幕", exact: true }).click();
  await page.waitForFunction(() => fixture.heldCommand);
  await openConnection(page);
  await page.getByRole("button", { name: "断开连接", exact: true }).click();
  await connect(page);
  await page.locator(".phone-screen").waitFor();
  const countBefore = await page.evaluate(
    () =>
      fixture.calls.filter(
        (c) => c.url?.includes("/session-2/") && c.url.endsWith("/preview"),
      ).length,
  );
  await page.evaluate(() => fixture.releaseCommand());
  await page.waitForFunction(
    () => !document.querySelector("#phone-text").disabled,
  );
  assert.equal(
    await page.evaluate(
      () =>
        fixture.calls.filter(
          (c) => c.url?.includes("/session-2/") && c.url.endsWith("/preview"),
        ).length,
    ),
    countBefore,
  );
});


test("task composer supports selecting, pasting, dropping and removing reference images", async t => {
  const page = await pageFor(t); await connect(page);
  const png = Buffer.from(await page.evaluate(() => {
    const canvas = document.createElement('canvas'); canvas.width = 16; canvas.height = 16;
    return canvas.toDataURL('image/png').split(',')[1];
  }), 'base64');
  await page.getByLabel('添加任务图片').setInputFiles({ name: 'reference.png', mimeType: 'image/png', buffer: png });
  await page.getByRole('img', { name: 'reference.png', exact: true }).waitFor();
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.items.add(new File([Uint8Array.from(atob(document.querySelector('.image-thumbnails img').src.split(',')[1]), c => c.charCodeAt(0))], 'pasted.png', { type: 'image/png' }));
    document.querySelector('#task-instruction').dispatchEvent(new ClipboardEvent('paste', { bubbles: true, clipboardData: data }));
  });
  await page.getByRole('img', { name: 'pasted.png', exact: true }).waitFor();
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.items.add(new File([Uint8Array.from(atob(document.querySelector('.image-thumbnails img').src.split(',')[1]), c => c.charCodeAt(0))], 'dropped.png', { type: 'image/png' }));
    document.querySelector('#task-instruction').dispatchEvent(new DragEvent('drop', { bubbles: true, dataTransfer: data }));
  });
  await page.getByRole('img', { name: 'dropped.png', exact: true }).waitFor();
  await page.getByRole('button', { name: '移除图片' }).last().click();
  await page.waitForFunction(() => document.querySelectorAll('.image-thumbnails img').length === 2);
  await page.locator('#task-instruction').fill('Use the supplied pictures');
  await page.getByRole('button', { name: /开始任务/ }).click();
  await page.waitForFunction(() => window.fixture.calls.some(c => c.url?.endsWith('/tasks')));
  const body = await page.evaluate(() => window.fixture.calls.find(c => c.url?.endsWith('/tasks')).body);
  assert.equal(body.attachments.length, 2);
  assert.ok(body.attachments.every(id => id.startsWith('image-')));
});


test('iPhone idle and readiness changes create no video or screenshot traffic; manual capture remains available', async t => {
  const page = await pageFor(t);
  await page.evaluate(() => { fixture.enableStream = true; });
  await connect(page, 'ios', false);
  await page.getByRole('heading', {name: '设备截图', exact: true}).waitFor();
  await page.waitForFunction(() => fixture.calls.filter(c => c.url?.endsWith('/session-1')).length >= 4);
  assert.equal(await page.locator('.phone-screen').count(), 0);
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.includes('/preview')).length), 0);
  await page.evaluate(() => { fixture.state.daemon.state.state = 'wda_starting'; });
  await page.waitForFunction(() => document.querySelector('.connection-status').textContent.includes('WDA 准备中'));
  await page.evaluate(() => { fixture.state.daemon.state.state = 'ready'; });
  await page.waitForFunction(() => document.querySelector('.connection-status').textContent.includes('可以控制'));
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.includes('/preview')).length), 0);
  await page.getByRole('button', {name: '刷新屏幕', exact: true}).click();
  await page.waitForFunction(() => document.querySelector('.phone-screen')?.naturalWidth === 100);
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith('/preview')).length), 1);
  await page.locator('.screen-meta').filter({hasText: '最近一次截图（非实时）'}).waitFor();
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.includes('/preview/stream')).length), 0);
});

test('on-demand screenshots retain rotation and manual gestures without a continuous viewer', async t => {
  const page = await pageFor(t);
  await connect(page);
  const image = page.locator('.phone-screen');
  const box = await image.boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await page.waitForFunction(() => fixture.calls.some(c => c.body?.tool === 'iphone_tap'));
  const tap = await page.evaluate(() => fixture.calls.find(c => c.body?.tool === 'iphone_tap').body);
  assert.ok(Math.abs(tap.args.x - 50) < 1); assert.ok(Math.abs(tap.args.y - 100) < 1);
  // Change the fixture's next image to landscape.
  await page.evaluate(() => {
    const canvas = document.createElement('canvas'); canvas.width = 200; canvas.height = 100;
    fixture.image.imageBase64 = canvas.toDataURL('image/jpeg').split(',')[1];
    fixture.freezeImage = true;
  });
  await page.getByRole('button', {name: '刷新屏幕', exact: true}).click();
  await page.waitForFunction(() => document.querySelector('.phone-frame')?.classList.contains('wide'));
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.includes('/preview/stream')).length), 0);
});

test('opt-in live preview maps MJPEG pixels to phone points and stops back to screenshots', async t => {
  const page = await pageFor(t);
  t.after(() => { streamFixtureBytes = undefined; });
  streamFixtureBytes = Buffer.from(await page.evaluate(() => {
    const canvas = document.createElement('canvas'); canvas.width = 400; canvas.height = 800;
    canvas.getContext('2d').fillRect(0, 0, 400, 800);
    return canvas.toDataURL('image/jpeg').split(',')[1];
  }), 'base64');
  await page.evaluate(() => {
    fixture.enableStream = true;
    fixture.state.daemon.persisted = { screen: { width: 100, height: 200 } };
    fixture.state.preview = { state: 'streaming', ageMs: 10 };
  });
  await connect(page, 'ios', false);
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.includes('/preview/stream')).length), 0);
  await page.getByRole('button', { name: '实时画面', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('.phone-screen')?.naturalWidth === 400);
  await page.getByRole('heading', { name: '实时画面', exact: true }).waitFor();
  const image = page.locator('.phone-screen');
  const box = await image.boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await page.waitForFunction(() => fixture.calls.some(c => c.body?.tool === 'iphone_tap'));
  const tap = await page.evaluate(() => fixture.calls.find(c => c.body?.tool === 'iphone_tap').body);
  assert.ok(Math.abs(tap.args.x - 50) < 1); assert.ok(Math.abs(tap.args.y - 100) < 1);
  await page.getByRole('button', { name: '停止实时', exact: true }).click();
  await page.waitForFunction(() => fixture.calls.some(c => c.url?.endsWith('/preview/stream') && c.method === 'DELETE'));
  await page.getByRole('heading', { name: '设备截图', exact: true }).waitFor();
});

test('a stale live preview is released and replaced by one screenshot', async t => {
  const page = await pageFor(t);
  t.after(() => { streamFixtureBytes = undefined; });
  streamFixtureBytes = Buffer.from(await page.evaluate(() => fixture.image.imageBase64), 'base64');
  await page.evaluate(() => { fixture.enableStream = true; fixture.state.preview = { state: 'streaming', ageMs: 10 }; });
  await connect(page, 'ios', false);
  await page.getByRole('button', { name: '实时画面', exact: true }).click();
  await page.locator('.screen-meta').filter({ hasText: '实时画面' }).waitFor();
  await page.evaluate(() => { fixture.state.preview = { state: 'waiting' }; });
  await page.getByText('实时画面暂停', { exact: false }).waitFor({ timeout: 12000 });
  await page.waitForFunction(() => document.querySelector('.phone-screen')?.src.startsWith('data:image/jpeg;base64,'));
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith('/preview/stream') && c.method === 'DELETE').length), 1);
});

test('each task observation is shown without the user refreshing', async t => {
  const page = await pageFor(t);
  await connect(page, 'ios', false);
  const previews = () => page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith('/preview')).length);
  assert.equal(await previews(), 0);
  await page.evaluate(() => {
    fixture.state.task = { id: 'task', instruction: 'Inspect', state: 'observing', observationId: 'o1', confirmation: null, steps: [] };
  });
  await page.locator('.task-status').waitFor();
  assert.equal(await previews(), 0, 'no capture while the task is still observing');
  await page.evaluate(() => { fixture.state.task = { ...fixture.state.task, state: 'thinking' }; });
  await page.waitForFunction(() => fixture.calls.filter(c => c.url?.endsWith('/preview')).length === 1);
  await page.locator('.phone-screen').waitFor();
  await page.locator('.screen-meta').filter({ hasText: '任务最近一次观察' }).waitFor();
  await page.evaluate(() => { fixture.state.task = { ...fixture.state.task, state: 'executing' }; });
  await page.evaluate(() => { fixture.state.task = { ...fixture.state.task, state: 'thinking', observationId: 'o2' }; });
  await page.waitForFunction(() => fixture.calls.filter(c => c.url?.endsWith('/preview')).length === 2);
});

test('each task step shows its observation, model and action time', async t => {
  const page = await pageFor(t);
  await connect(page);
  await page.evaluate(() => {
    fixture.state.task = {
      id: 'task', instruction: 'Inspect', state: 'thinking', confirmation: null, observationId: 'o2',
      timings: { observationMs: 700, modelMs: 4490 },
      steps: [
        { id: 's1', tool: 'iphone_tap', reason: 'Open search', state: 'succeeded', timings: { observationMs: 904, modelMs: 3719, actionMs: 852 } },
        { id: 's2', tool: 'iphone_type_text', reason: 'Search', state: 'executing', timings: null },
      ],
    };
  });
  await page.locator('.step-timing').first().waitFor();
  assert.equal(await page.locator('.step-timing').count(), 1, 'a step without timings shows none');
  assert.equal(await page.locator('.step-timing').textContent(), '观察 0.90 秒 · 模型 3.72 秒 · 操作 0.85 秒');
});

test('screenshots load from screen.jpg as object URLs, revoke the previous one, and fall back to base64 without the route', async t => {
  const page = await pageFor(t);
  await page.evaluate(() => { fixture.serveScreenJpeg = true; });
  await connect(page);
  const image = page.locator('.phone-screen');
  const first = await image.getAttribute('src');
  assert.match(first, /^blob:/);
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith('/screen.jpg')).length), 1);
  await page.getByRole('button', {name: '刷新屏幕', exact: true}).click();
  await page.waitForFunction(source => document.querySelector('.phone-screen')?.getAttribute('src') !== source, first);
  assert.match(await image.getAttribute('src'), /^blob:/);
  assert.equal(await page.evaluate(src => fetch(src).then(() => 'alive', () => 'revoked'), first), 'revoked');
  await page.waitForFunction(() => document.querySelector('.phone-screen')?.naturalWidth === 100);
  // Older gateway: the route answers JSON, so the base64 image is used and the
  // route is not requested again for this session.
  await page.evaluate(() => { fixture.serveScreenJpeg = false; });
  const jpegCalls = await page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith('/screen.jpg')).length);
  for (let i = 0; i < 2; i++) {
    const before = await image.getAttribute('src');
    await page.getByRole('button', {name: '刷新屏幕', exact: true}).click();
    await page.waitForFunction(source => document.querySelector('.phone-screen')?.getAttribute('src') !== source, before);
    assert.match(await image.getAttribute('src'), /^data:image\/jpeg;base64,/);
    await page.waitForFunction(() => !document.querySelector('#phone-text').disabled);
  }
  assert.equal(await page.evaluate(() => fixture.calls.filter(c => c.url?.endsWith('/screen.jpg')).length), jpegCalls + 1);
});
