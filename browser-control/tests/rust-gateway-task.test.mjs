import assert from 'node:assert/strict';
import { fixtureImageBase64 } from './fixtures/screen-image.mjs';
import http from 'node:http';
import WebSocket from 'ws';
import { encodeFrame, decodeFrame, FrameType as F } from './support/relay-protocol.mjs';
import {
  contract, pause, until, harness, peer, ready, modelReplyTask, modelCompletion, nativeCounts,
} from './helpers/rust-gateway.mjs';

// AI task loop: WDA recovery, typing focus, plans, goal updates, confirmation and control ownership.

contract('WDA recovery waits for readiness and a fresh observation without replaying an uncertain tap', async t => {
  for (const interrupted of [false, true]) {
    await t.test(interrupted ? 'interrupted native response' : 'native WDA timeout', async t => {
      const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
        ? { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen' }
        : { type: 'done', message: 'Fresh screen verified' })), {
        FIXTURE_TAP_WDA_FAILURES: '1', FIXTURE_WDA_RECOVERY_MS: '600',
        FIXTURE_RECOVERY_OBSERVATION_FAILURES: '1', ...(interrupted ? { FIXTURE_TAP_INTERRUPTED: '1' } : {}),
      });
      let state;
      await until(async () => { state = await fixture.snapshot(); return ['recovering', 'failed'].includes(state.task?.state); });
      assert.equal(state.task.state, 'recovering', JSON.stringify(state.task));
      assert.equal(state.task.steps[0].state, 'uncertain');
      assert.equal(state.task.recovery.maxWaitSeconds, 120);
      await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); }, 600);
      assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
      assert.equal(state.task.steps.length, 1);
      assert.equal(state.task.steps[0].state, 'uncertain');
      assert.equal(fixture.requests.length, 2);
      const context = JSON.parse(fixture.requests[1].messages[1].content[0].text);
      assert.equal(context.recentHistory[0].state, 'uncertain');
      assert.equal(context.observation.caption, 'fixture fresh screen after action');
      const counters = await nativeCounts(fixture);
      assert.equal(counters.taps, 1);
      assert.ok(counters.counts.screenshot >= 3);
    });
  }
});

contract('WDA recovery is cancelled promptly by stop or device disconnection', async t => {
  for (const disconnect of [false, true]) {
    await t.test(disconnect ? 'device disconnect' : 'user stop', async t => {
      const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
        type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen',
      })), { FIXTURE_TAP_WDA_FAILURES: '1', FIXTURE_WDA_RECOVERY_MS: '300000' });
      let state;
      await until(async () => { state = await fixture.snapshot(); return ['recovering', 'failed'].includes(state.task?.state); });
      assert.equal(state.task.state, 'recovering', JSON.stringify(state.task));
      if (disconnect) {
        assert.equal((await fixture.app.api(fixture.route, fixture.session.token, undefined, {}, 'DELETE')).status, 200);
      } else {
        const stopped = await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/stop`, fixture.session.token, {});
        assert.equal((await stopped.json()).state, 'stopped');
        await pause(2200);
        assert.equal((await fixture.snapshot()).task.state, 'stopped');
      }
      assert.equal(fixture.requests.length, 1);
    });
  }
});

contract('WDA recovery preserves unrelated failures and uncertain confirmed actions', async t => {
  for (const confirmed of [false, true]) {
    await t.test(confirmed ? 'confirmed action needs manual outcome check' : 'unrelated error remains fatal', async t => {
      const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
        type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen', requiresConfirmation: confirmed,
      })), { FIXTURE_TAP_WDA_FAILURES: '1', ...(confirmed ? {} : { FIXTURE_TAP_ERROR: 'Fixture validation failure' }) });
      if (confirmed) {
        let waiting;
        await until(async () => { waiting = await fixture.snapshot(); return waiting.task?.state === 'awaiting_confirmation'; });
        await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/confirm`, fixture.session.token, {
          actionId: waiting.task.confirmation.actionId, approved: true,
        });
      }
      let state;
      await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
      assert.equal(state.task.steps[0].state, confirmed ? 'uncertain' : 'failed');
      assert.equal(fixture.requests.length, 1);
      assert.match(state.task.error, confirmed ? /结果不确定|uncertain/i : /Fixture validation failure/);
    });
  }
});

contract('WDA recovery has a two-episode task limit while retaining uncertain steps', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_tap', args: { x: call, y: 7 }, reason: 'Inspect a different fixture target',
  })), { FIXTURE_TAP_WDA_FAILURES: '3' });
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  assert.equal(state.task.steps.length, 3);
  assert.ok(state.task.steps.every(step => step.state === 'uncertain'));
  assert.match(state.task.error, /恢复次数|recovery limit/i);
  assert.equal(fixture.requests.length, 3);
});

contract('WDA recovery rejects a fresh observation from a different device', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen',
  })), { FIXTURE_TAP_WDA_FAILURES: '1', FIXTURE_RECOVERY_FOREIGN_OBSERVATION: '1' });
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  assert.match(state.task.error, /identity/);
  assert.equal(state.task.steps[0].state, 'uncertain');
  assert.equal(fixture.requests.length, 1);
});

contract('WDA recovery handles an unavailable initial observation before calling the model', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'Fresh fixture observed' })), {
    FIXTURE_INITIAL_OBSERVATION_WDA_FAILURES: '1',
  });
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.steps.length, 0);
  assert.equal(state.task.recovery.attempt, 1);
  assert.equal(fixture.requests.length, 1);
});

contract('WDA recovery retries an unavailable observation after a successful navigation', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
    ? { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Return to fixture album list' }
    : { type: 'done', message: 'Fresh album screen verified' })), {
    FIXTURE_TAP_WDA_FAILURES: '0', FIXTURE_RECOVERY_OBSERVATION_FAILURES: '1',
    FIXTURE_TAP_ERROR: 'Fixture screenshot unavailable after navigation',
  });
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.steps.length, 1);
  assert.equal(state.task.steps[0].state, 'succeeded');
  assert.equal(state.task.recovery.attempt, 1);
  const context = JSON.parse(fixture.requests[1].messages[1].content[0].text);
  assert.equal((await nativeCounts(fixture)).taps, 1);
});

contract('WDA recovery requires checking uncertain text input before continuing', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_type_text', args: { text: 'fixture text' }, reason: 'Fill fixture input',
  })), { FIXTURE_TAP_WDA_FAILURES: '1', FIXTURE_TYPE_TEXT_ERROR: 'WDA 请求超时（请求已发出，手机可能仍在执行，先截图确认）：WDA request timed out' });
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  assert.equal(state.task.steps[0].state, 'uncertain');
  assert.match(state.task.error, /结果不确定/);
  assert.equal(fixture.requests.length, 1);
});

contract('WDA recovery ends after the actual 120-second deadline', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen',
  })), { FIXTURE_TAP_WDA_FAILURES: '1', FIXTURE_WDA_RECOVERY_MS: '300000' });
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['recovering', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'recovering');
  await pause(119000);
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  assert.match(state.task.error, /120/);
  assert.equal(state.task.steps[0].state, 'uncertain');
  assert.ok(state.task.recovery.elapsedMs >= 120000);
  assert.ok(state.task.recovery.elapsedMs < 125000);
  assert.equal(fixture.requests.length, 1);
});

contract('native action failure exposes its bounded cause and never retries typing', async t => {
  const reason = 'No keyboard on screen; tap a text field first. ' + 'detail '.repeat(500);
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_type_text', args: { text: 'fixture draft', submit: false }, reason: 'Enter the draft',
  })), { FIXTURE_TYPE_TEXT_ERROR: reason });
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  assert.match(state.task.error, /No keyboard on screen/);
  assert.ok(state.task.error.length < 2200);
  assert.equal(state.task.steps.length, 1);
  assert.equal(state.task.steps[0].state, 'failed');
  assert.equal(fixture.requests.length, 1);
});

const missingKeyboard = 'No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up';
const typeDraft = { type: 'action', tool: 'iphone_type_text', args: { text: 'fixture draft', submit: false }, reason: 'Enter the draft' };
const focusField = { type: 'action', tool: 'iphone_tap', args: { x: 20, y: 50 }, reason: 'Focus the draft field' };

contract('typing focus recovers only after a fresh observation and a focus tap', async t => {
  for (const reason of [missingKeyboard, '屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入']) {
    await t.test(reason.startsWith('No') ? 'English' : 'Chinese', async t => {
      const actions = [typeDraft, focusField, typeDraft, { type: 'done', message: 'Draft entered' }];
      const fixture = await modelReplyTask(t, n => modelCompletion(JSON.stringify(actions[n - 1])), {
        FIXTURE_TYPE_TEXT_NEEDS_FOCUS: reason,
      });
      let state;
      await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'completed'; });
      assert.deepEqual(state.task.steps.map(step => step.state), ['failed', 'succeeded', 'succeeded']);
      const input = JSON.parse(fixture.requests[1].messages[1].content[0].text);
      assert.equal(input.recentHistory[0].result.message, reason);
      assert.ok(JSON.parse(fixture.requests[1].messages[1].content[0].text).observation.ocr.lines.length > 0);
      assert.ok(fixture.requests[1].messages[1].content.every(part => part.type === 'text'));
      const counts = await nativeCounts(fixture);
      assert.equal(counts.typeRequests, 2);
      assert.equal(counts.taps, 1);
      assert.equal(counts.counts.screenshot, 4);
    });
  }
});

contract('typing focus returns blind typing to the model before dispatch, then halts at the re-plan limit', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify(typeDraft)), {
    FIXTURE_TYPE_TEXT_ERROR: missingKeyboard,
  });
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  // One dispatched (failed) typing, two blind attempts returned for re-planning, the third halts.
  assert.equal(fixture.requests.length, 4);
  assert.equal(state.task.steps.length, 1);
  assert.equal((await nativeCounts(fixture)).typeRequests, 1);
  assert.match(state.task.error, /focus/i);
  const replan = JSON.parse(fixture.requests[2].messages[1].content[0].text).recentHistory.at(-1);
  assert.equal(replan.state, 'rejected');
  assert.match(replan.result.message, /Tap the intended text field first/);
});

contract('typing focus stops after one unsuccessful focus recovery', async t => {
  const fixture = await modelReplyTask(t, n => modelCompletion(JSON.stringify(n === 2 ? focusField : typeDraft)), {
    FIXTURE_TYPE_TEXT_ERROR: missingKeyboard,
  });
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed' && state.task.steps.length === 3; });
  assert.equal(fixture.requests.length, 3);
  assert.equal(state.task.error, missingKeyboard);
  assert.equal((await nativeCounts(fixture)).typeRequests, 2);
});

contract('typing focus cannot report completion while the failed input is unresolved', async t => {
  for (const tapped of [false, true]) {
    await t.test(tapped ? 'after focus tap' : 'before focus tap', async t => {
      const actions = [typeDraft, ...(tapped ? [focusField] : []), { type: 'done', message: 'Draft entered' }];
      const fixture = await modelReplyTask(t, n => modelCompletion(JSON.stringify(actions[n - 1])), {
        FIXTURE_TYPE_TEXT_ERROR: missingKeyboard,
      });
      let state;
      await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
      assert.equal(fixture.requests.length, actions.length);
      assert.equal((await nativeCounts(fixture)).typeRequests, 1);
    });
  }
});

contract('AI previews reuse its observation and query text plus screenshot crops reach the next model call', async t => {
  let calls = 0, release;
  const bodies = [];
  const provider = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    bodies.push(JSON.parse(body)); calls++;
    const reply = calls === 1
      ? { type: 'action', tool: 'iphone_list_apps', args: {}, reason: 'Find fixture app' }
      : calls === 2 ? { type: 'action', tool: 'iphone_screenshot', args: { x: 0, y: 0, width: 100, height: 100 }, reason: 'Read crop' }
      : { type: 'done', message: 'Fixture completed' };
    const send = () => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ choices: [{ message: { content: JSON.stringify(reply) } }] })); };
    if (calls === 1) release = send; else send();
  });
  await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
  t.after(() => { provider.closeAllConnections(); provider.close(); });
  const app = await harness(t, { LLM_BASE_URL: `http://127.0.0.1:${provider.address().port}/v1`, LLM_MODEL: 'fixture', LLM_API_KEY: 'fixture-only' });
  const s = await app.session(); await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  await app.api(route + '/tasks', s.token, { instruction: 'Inspect fixture app' });
  await until(() => !!release);
  const count = async requestId => JSON.parse((await (await app.api(route + '/command', s.token, { requestId, deviceId: s.deviceId, tool: 'iphone_status' })).json()).text).counts;
  const before = await count('before-preview');
  const previews = await Promise.all(Array.from({ length: 12 }, () => app.api(route + '/preview', s.token).then(r => r.json())));
  assert.ok(previews.every(p => p.imageBase64 === fixtureImageBase64));
  assert.deepEqual(await count('after-preview'), before, 'active AI previews must not issue screenshot or element requests');
  release();
  let state;
  await until(async () => { state = await (await app.api(route, s.token)).json(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task)); assert.equal(calls, 3);
  const firstObservation = JSON.parse(bodies[0].messages[1].content[0].text).observation;
  assert.equal(firstObservation.ocr.coordinateSpace, 'phonePoints');
  assert.equal(firstObservation.ocr.lines[0].text, 'Fixture screen');
  assert.equal(firstObservation.elementsText, undefined);
  const second = JSON.parse(bodies[1].messages[1].content[0].text);
  assert.match(second.recentHistory[0].result.text, /com.example.fixture/);
  assert.equal(bodies[1].messages[1].content.filter(p => p.type === 'image_url').length, 0, 'read-only text lookup needs no new image');
  assert.equal(bodies[2].messages[1].content.filter(p => p.type === 'image_url').length, 0);
  assert.equal(JSON.parse(bodies[2].messages[1].content[0].text).observation.ocr.lines[0].text, 'Fixture screen');
  assert.deepEqual(await count('final-counts'), { screenshot: 2, list_elements: 0, launch_app: 0, terminate_app: 0, press_button: 0 });
  assert.ok(!JSON.stringify(state.task).includes('com.example.fixture'), 'tool contents stay out of polled UI snapshots');
  assert.ok(state.task.timings.modelMs >= 0);
  assert.ok(state.task.steps[0].timings.actionMs >= 0);
  t.diagnostic('12 preview requests during model thinking added 0 device screenshot/element reads');
});

contract('exact home command skips initial model planning but still observes and verifies its result', async t => {
  const bodies = [];
  const provider = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk; bodies.push(JSON.parse(body));
    res.setHeader('Content-Type','application/json'); res.end(JSON.stringify({ choices:[{ message:{ content:JSON.stringify({ type:'done',message:'Fixture verified home' }) } }] }));
  });
  await new Promise(resolve => provider.listen(0,'127.0.0.1',resolve));
  t.after(() => { provider.closeAllConnections(); provider.close(); });
  const app = await harness(t, { LLM_BASE_URL:`http://127.0.0.1:${provider.address().port}/v1`, LLM_MODEL:'fixture', LLM_API_KEY:'fixture' });
  const s = await app.session(); await peer(t,s); await ready(app,s); const route = `/api/sessions/${s.id}`;
  await app.api(route + '/tasks',s.token,{ instruction:'回到主屏幕' });
  let state; await until(async () => { state = await (await app.api(route,s.token)).json(); return ['completed','failed'].includes(state.task?.state); });
  assert.equal(state.task.state,'completed',JSON.stringify(state.task)); assert.equal(bodies.length,1);
  const input = JSON.parse(bodies[0].messages[1].content[0].text);
  assert.equal(input.recentHistory[0].tool,'iphone_press_button'); assert.equal(input.recentHistory[0].args.button,'home');
  assert.equal(state.task.steps[0].timings.direct,true);
  assert.equal(bodies[0].messages[1].content.filter(p => p.type === 'image_url').length,0);
});

contract('query-only evidence cannot dispatch coordinate taps until a fresh image is obtained', async t => {
  const bodies = [];
  const replies = [
    { type:'action',tool:'iphone_list_apps',args:{},reason:'Read apps' },
    { type:'action',tool:'iphone_tap',args:{ x:999,y:999 },reason:'Unverified coordinates' },
    { type:'action',tool:'iphone_tap',args:{ x:5,y:7 },reason:'Coordinates from fresh observation' },
    { type:'done',message:'Fixture completed' },
  ];
  const provider = http.createServer(async (req,res) => {
    let body = ''; for await (const chunk of req) body += chunk; bodies.push(JSON.parse(body));
    res.setHeader('Content-Type','application/json'); res.end(JSON.stringify({ choices:[{ message:{ content:JSON.stringify(replies[bodies.length-1] || { type:'done',message:'Unexpected extra call' }) } }] }));
  });
  await new Promise(resolve => provider.listen(0,'127.0.0.1',resolve)); t.after(() => { provider.closeAllConnections(); provider.close(); });
  const app = await harness(t,{ LLM_BASE_URL:`http://127.0.0.1:${provider.address().port}/v1`,LLM_MODEL:'fixture',LLM_API_KEY:'fixture' });
  const s = await app.session(); await peer(t,s); await ready(app,s); const route = `/api/sessions/${s.id}`;
  await app.api(route + '/tasks',s.token,{ instruction:'Inspect fixture and tap' });
  let state; await until(async () => { state = await (await app.api(route,s.token)).json(); return ['completed','failed'].includes(state.task?.state); });
  assert.equal(state.task.state,'completed',JSON.stringify(state.task)); assert.equal(bodies.length,4);
  assert.equal(bodies[1].messages[1].content.filter(p => p.type === 'image_url').length,0);
  assert.equal(bodies[2].messages[1].content.filter(p => p.type === 'image_url').length,0);
  assert.deepEqual(state.task.steps.filter(p => p.tool === 'iphone_tap').map(p => p.args),[{x:5,y:7}]);
});

contract('Rust task calls OCR text provider, confirms once, completes and cancels pending generation', async t => {
  let calls = 0, lastBody, hanging = false;
  const provider = http.createServer(async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    lastBody = JSON.parse(body); calls++;
    if (hanging) return;
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ choices: [{ message: { content: JSON.stringify(calls === 1
      ? { type: 'action', tool: 'iphone_type_text', args: { text: 'hello', submit: true }, reason: 'Submit text' }
      : { type: 'done', message: 'Finished' }) } }] }));
  });
  await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
  t.after(() => { provider.closeAllConnections(); provider.close(); });
  const app = await harness(t, { LLM_BASE_URL: `http://127.0.0.1:${provider.address().port}/v1`, LLM_MODEL: 'fixture-vision', LLM_API_KEY: 'fixture-secret-key' });
  const s = await app.session(); await peer(t, s); await ready(app, s); const route = `/api/sessions/${s.id}`;
  const started = await app.api(route + '/tasks', s.token, { instruction: 'submit fixture' }); assert.equal(started.status, 201); const task = await started.json();
  let state; await until(async () => { state = await (await app.api(route, s.token)).json(); return state.task?.confirmation; });
  assert.ok(lastBody.messages[1].content.every(part => part.type === 'text'));
  assert.equal(JSON.parse(lastBody.messages[1].content[0].text).observation.ocr.lines[0].text, 'Fixture screen');
  assert.equal((await app.api(`${route}/tasks/${task.id}/confirm`, s.token, { actionId: 'stale', approved: true })).status, 409);
  const decision = { actionId: state.task.confirmation.actionId, approved: true };
  assert.equal((await app.api(`${route}/tasks/${task.id}/confirm`, s.token, decision)).status, 200);
  assert.equal((await app.api(`${route}/tasks/${task.id}/confirm`, s.token, decision)).status, 409);
  await until(async () => { state = await (await app.api(route, s.token)).json(); return state.task?.state === 'completed'; });
  assert.equal(state.task.steps[0].state, 'succeeded'); assert.ok(!JSON.stringify(state).includes('fixture-secret-key'));
  hanging = true;
  const next = await (await app.api(route + '/tasks', s.token, { instruction: 'wait' })).json();
  await until(() => calls === 3);
  const stopped = await app.api(`${route}/tasks/${next.id}/stop`, s.token, {}); assert.equal(stopped.status, 200); assert.equal((await stopped.json()).state, 'stopped');
});

contract('tasks have no step limit and run past one hundred actions until completion', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call > 120
    ? { type: 'done', message: '120 verified' }
    : { type: 'action', tool: 'iphone_status', args: {}, reason: 'Read fixture status' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); }, 1500);
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task.error));
  assert.equal(state.task.steps.length, 120);
  assert.equal(state.task.message, '120 verified');
  assert.equal(state.task.maxSteps, undefined);
});

contract('task attachments validate image bytes, enforce ownership and reach model as separate references', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'Reference verified' })));
  await until(async () => (await fixture.snapshot()).task?.state === 'completed');
  const app = fixture.app, s = fixture.session, route = fixture.route;
  const photo = { name: 'posting.png', mimeType: 'image/png', dataBase64: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC' };
  assert.equal((await app.api(route + '/attachments', 'wrong', photo)).status, 401);
  assert.equal((await app.api(route + '/attachments', s.token, { ...photo, dataBase64: 'dGV4dA==' })).status, 400);
  const response = await app.api(route + '/attachments', s.token, photo);
  assert.equal(response.status, 201, await response.clone().text());
  const attachment = await response.json();
  assert.equal(attachment.name, 'posting.png'); assert.equal(attachment.mimeType, 'image/png');
  const other = await app.session();
  assert.equal((await app.api(`/api/sessions/${other.id}/tasks`, other.token, { instruction: 'Inspect', attachments: [attachment.id] })).status, 404);
  const start = await app.api(route + '/tasks', s.token, { instruction: 'Inspect reference picture', attachments: [attachment.id] });
  assert.equal(start.status, 201);
  await until(async () => (await fixture.snapshot()).task?.state === 'completed');
  const payload = fixture.requests.at(-1);
  const content = payload.messages[1].content;
  assert.equal(content.filter(c => c.type === 'image_url').length, 0);
  const input = JSON.parse(content[0].text);
  assert.equal(input.referenceImages[0].ocr.lines[0].text, 'Fixture screen');
  assert.equal(input.referenceImages[0].ocr.coordinateSpace, 'referenceImagePixels');
  assert.ok(!JSON.stringify(payload).includes(photo.dataBase64));
  assert.ok(!JSON.stringify(payload).includes(fixtureImageBase64));
  assert.ok(!JSON.stringify(payload).includes('phoneImageUrl'));
  assert.match(JSON.stringify(content), /posting.png/);
  const state = await fixture.snapshot();
  assert.ok(!JSON.stringify(state).includes(photo.dataBase64));
  assert.equal((await app.api(route + '/attachments/' + attachment.id, s.token, undefined, {}, 'DELETE')).status, 200);
  assert.equal((await app.api(route + '/tasks', s.token, { instruction: 'Inspect', attachments: [attachment.id] })).status, 404);
});

contract('control boundaries force bounded capture despite low latency opt-out', async t => {
  const app = await harness(t, { ICTL_LOW_LATENCY: '0', ICTL_CAPTURE_POLICY: 'local' });
  const session = await app.session(); await peer(t, session); await ready(app, session);
  const response = await app.api(`/api/sessions/${session.id}/command`, session.token, { deviceId: session.deviceId, requestId: 'capture-policy', tool: 'iphone_status' });
  const echo = JSON.parse((await response.json()).text);
  assert.equal(echo.env.ICTL_LOW_LATENCY, '0');
  assert.equal(echo.env.ICTL_CAPTURE_POLICY, 'bounded-remote');
});

contract('control boundaries hold task ownership through model and confirmation waits', async t => {
  for (const confirmation of [false, true]) await t.test(confirmation ? 'confirmation wait' : 'model wait', async t => {
    let release;
    const modelGate = new Promise(resolve => { release = resolve; });
    const action = { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen', requiresConfirmation: confirmation };
    const fixture = await modelReplyTask(t, async call => {
      if (call === 1 && !confirmation) await modelGate;
      return modelCompletion(JSON.stringify(call === 1 ? action : { type: 'done', message: 'Verified fixture' }));
    });
    let state;
    await until(async () => { state = await fixture.snapshot(); return confirmation ? state.task?.state === 'awaiting_confirmation' : fixture.requests.length === 1; });
    const command = (requestId, tool, args = {}) => fixture.app.api(fixture.route + '/command', fixture.session.token, { requestId, deviceId: fixture.session.deviceId, tool, args, taskId: fixture.task.id, action_deadline_monotonic_ns: Number.MAX_SAFE_INTEGER });
    const before = JSON.parse((await (await command('before', 'iphone_status')).json()).text);
    for (const [tool, args] of [ ['iphone_tap', { x: 5, y: 7 }], ['iphone_launch_app', { app: 'com.example.fixture' }], ['iphone_screenshot', {}], ['iphone_list_elements', {}], ['iphone_wait_for', { text: 'fixture' }] ]) {
      const denied = await command('denied-' + tool, tool, args);
      assert.equal(denied.status, 409, await denied.clone().text());
    }
    const setup = await fixture.app.api(fixture.route + '/setup', fixture.session.token, {});
    assert.equal(setup.status, 409, await setup.clone().text());
    assert.match((await setup.json()).error, /owns session control/i);
    const setupInput = await fixture.app.api(fixture.route + '/setup/input', fixture.session.token, { line: 'fixture input' });
    assert.equal(setupInput.status, 409);
    assert.match((await setupInput.json()).error, /owns session control/i);
    assert.equal((await fixture.app.api(fixture.route + '/screen', fixture.session.token)).status, 409);
    assert.equal((await fixture.app.api(fixture.route + '/preview', fixture.session.token)).status, 200);
    const after = JSON.parse((await (await command('after', 'iphone_status')).json()).text);
    assert.equal(after.taps, before.taps);
    assert.deepEqual(after.counts, before.counts);
    if (confirmation) {
      assert.equal((await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/confirm`, fixture.session.token, { actionId: state.task.confirmation.actionId, approved: true })).status, 200);
    } else release();
    await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
    assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
    const fresh = await command('manual-after-release', 'iphone_tap', { x: 3, y: 4 });
    assert.equal(fresh.status, 200, await fresh.clone().text());
    const result = await fresh.json();
    assert.equal(JSON.parse(result.text).taps, 2);
    const duplicate = await command('manual-after-release', 'iphone_tap', { x: 3, y: 4 });
    assert.deepEqual(await duplicate.json(), result);
  });
});

contract('control boundaries prevent task claim from overtaking admitted manual observation', async t => {
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  const fixture = await modelReplyTask(t, async () => { await gate; return modelCompletion(JSON.stringify({ type: 'done', message: 'Cancelled initial fixture' })); }, { FIXTURE_ELEMENTS_DELAY_MS: '700' });
  await until(() => fixture.requests.length === 1);
  await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/stop`, fixture.session.token, {});
  release();
  await pause(30);
  const screen = fixture.app.api(fixture.route + '/screen', fixture.session.token);
  await pause(80);
  const blocked = await fixture.app.api(fixture.route + '/tasks', fixture.session.token, { instruction: 'Inspect queued control fairly' });
  assert.equal(blocked.status, 409, await blocked.clone().text());
  assert.equal((await screen).status, 200);
  const fresh = await fixture.app.api(fixture.route + '/tasks', fixture.session.token, { instruction: 'Inspect after manual completion' });
  assert.equal(fresh.status, 201, await fresh.clone().text());
});

contract('control boundaries anchor action age before reading the HTTP body', async t => {
  const app = await harness(t); const session = await app.session();
  await peer(t, session); await ready(app, session);
  const payload = JSON.stringify({ deviceId: session.deviceId, requestId: 'slow-http-body', tool: 'iphone_tap', args: { x: 5, y: 7 } });
  const response = await new Promise((resolve, reject) => {
    const request = http.request(app.base + `/api/sessions/${session.id}/command`, { method: 'POST', headers: { Authorization: `Bearer ${session.token}`, 'Content-Type': 'application/json' } }, res => {
      let body = ''; res.on('data', bytes => { body += bytes; }); res.on('end', () => resolve({ status: res.statusCode, body: JSON.parse(body) }));
    });
    request.on('error', reject); request.write(payload.slice(0, 12));
    setTimeout(() => request.end(payload.slice(12)), 150);
  });
  assert.equal(response.status, 200);
  assert.ok(response.body.timings.queueMs >= 100, `HTTP body wait missing from action age: ${JSON.stringify(response.body.timings)}`);
});

contract('control boundaries keep setup child admission until input and completion', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'Initial task complete' })));
  await until(async () => (await fixture.snapshot()).task?.state === 'completed');
  await pause(30);
  assert.equal((await fixture.app.api(fixture.route + '/setup', fixture.session.token, {})).status, 200);
  const blocked = await fixture.app.api(fixture.route + '/tasks', fixture.session.token, { instruction: 'Inspect while setup waits' });
  assert.equal(blocked.status, 409, await blocked.clone().text());
  assert.equal((await fixture.app.api(fixture.route + '/setup/input', fixture.session.token, { line: 'fixture input' })).status, 200);
  await until(async () => (await fixture.snapshot()).setup?.state === 'complete');
  const fresh = await fixture.app.api(fixture.route + '/tasks', fixture.session.token, { instruction: 'Inspect after setup completion' });
  assert.equal(fresh.status, 201, await fresh.clone().text());
});

contract('control boundaries retain task ownership during WDA recovery and release on stop', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Inspect fixture screen' })), { FIXTURE_TAP_WDA_FAILURES: '1', FIXTURE_WDA_RECOVERY_MS: '300000' });
  await until(async () => (await fixture.snapshot()).task?.state === 'recovering');
  const command = (requestId, tool, args = {}) => fixture.app.api(fixture.route + '/command', fixture.session.token, { requestId, deviceId: fixture.session.deviceId, tool, args });
  const blocked = await command('during-recovery', 'iphone_tap', { x: 3, y: 4 });
  assert.equal(blocked.status, 409);
  assert.equal(JSON.parse((await (await command('recovery-status', 'iphone_status')).json()).text).taps, 1);
  await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/stop`, fixture.session.token, {});
  await pause(30);
  const fresh = await command('after-recovery-stop', 'iphone_tap', { x: 3, y: 4 });
  assert.equal(fresh.status, 200, await fresh.clone().text());
  assert.equal(JSON.parse((await fresh.json()).text).taps, 2);
  assert.equal(fixture.requests.length, 1);
});

contract('control boundaries block websocket pairing setup while a task owns control', async t => {
  let release; const gate = new Promise(resolve => { release = resolve; });
  const fixture = await modelReplyTask(t, async () => { await gate; return modelCompletion(JSON.stringify({ type: 'done', message: 'Fixture done' })); });
  await until(() => fixture.requests.length === 1);
  const errors = [];
  fixture.usb.ws.on('message', bytes => { const frame = decodeFrame(bytes); if (frame.type === F.Error && frame.requestId === 55) errors.push(JSON.parse(frame.payload.toString())); });
  fixture.usb.ws.send(encodeFrame({ type: F.StartPairing, requestId: 55 }));
  await pause(150);
  assert.equal((await fixture.snapshot()).setup.state, 'idle', 'pairing must not start native setup while task owns session');
  assert.equal(errors.length, 1);
  assert.match(errors[0].error, /control/i);
  assert.equal(fixture.usb.ws.readyState, WebSocket.OPEN);
  release();
  await until(async () => (await fixture.snapshot()).task?.state === 'completed');
});

contract('a model plan runs predictable actions from one observation and one model call', async t => {
  const plan = { type: 'plan', actions: [
    { tool: 'iphone_tap', args: { x: 20, y: 50 }, reason: 'Focus the fixture field' },
    { tool: 'iphone_type_text', args: { text: 'fixture', submit: false }, reason: 'Enter the fixture text' },
    { tool: 'iphone_press_button', args: { button: 'home' }, reason: 'Return home' },
  ] };
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1 ? plan : { type: 'done', message: 'Plan verified' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.deepEqual(state.task.steps.map(step => [step.tool, step.state, step.planned]), [
    ['iphone_tap', 'succeeded', false], ['iphone_type_text', 'succeeded', true], ['iphone_press_button', 'succeeded', true],
  ]);
  assert.equal(fixture.requests.length, 2);
  assert.deepEqual(state.task.plan, []);
  const history = JSON.parse(fixture.requests[1].messages[1].content[0].text).recentHistory;
  assert.deepEqual(history.map(entry => entry.tool), ['iphone_tap', 'iphone_type_text', 'iphone_press_button']);
});

contract('a model plan stops before a later coordinate action and re-observes', async t => {
  const plan = { type: 'plan', actions: [
    { tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Open the fixture item' },
    { tool: 'iphone_tap', args: { x: 6, y: 8 }, reason: 'Stale coordinate fixture tap' },
  ] };
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1 ? plan : { type: 'done', message: 'Re-observed' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.deepEqual(state.task.steps.map(step => step.args), [{ x: 5, y: 7 }]);
  assert.equal(fixture.requests.length, 2);
  assert.equal((await nativeCounts(fixture)).taps, 1);
});

contract('a goal update during planning re-plans from the same screen for the new goal', async t => {
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  t.after(() => release());
  const fixture = await modelReplyTask(t, async call => {
    if (call === 1) {
      await gate;
      return modelCompletion(JSON.stringify({ type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Old goal tap' }));
    }
    return modelCompletion(JSON.stringify({ type: 'done', message: 'New goal verified' }));
  });
  await until(() => fixture.requests.length === 1);
  const url = `${fixture.route}/tasks/${fixture.task.id}/instruction`;
  for (const invalid of [{}, { instruction: '' }, { instruction: 'x'.repeat(4001) }, { instruction: 'ok', extra: true }]) {
    assert.equal((await fixture.app.api(url, fixture.session.token, invalid)).status, 400, JSON.stringify(invalid).slice(0, 40));
  }
  assert.equal((await fixture.app.api(`${fixture.route}/tasks/missing/instruction`, fixture.session.token, { instruction: 'x' })).status, 404);
  const updated = await fixture.app.api(url, fixture.session.token, { instruction: 'New fixture goal' });
  assert.equal(updated.status, 200, await updated.clone().text());
  assert.equal((await updated.json()).instruction, 'New fixture goal');
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.message, 'New goal verified');
  assert.equal(state.task.steps.length, 0);
  assert.equal(state.task.instructionUpdates.length, 1);
  assert.equal(state.task.instructionUpdates[0].previous, 'Inspect the fixture safely');
  // The first done after a goal update is returned once so the model plans for the new goal.
  assert.equal(fixture.requests.length, 3);
  assert.equal(JSON.parse(fixture.requests[2].messages[1].content[0].text).recentHistory.at(-1).state, 'rejected');
  const instruction = JSON.parse(fixture.requests[1].messages[1].content[0].text).instruction;
  assert.match(instruction, /CURRENT GOAL[^\n]*New fixture goal/);
  assert.match(instruction, /Inspect the fixture safely/);
  const counts = await nativeCounts(fixture);
  assert.equal(counts.taps, 0);
  assert.equal(counts.counts.screenshot, 1, 'the second plan reuses the first observation');
  assert.equal((await fixture.app.api(url, fixture.session.token, { instruction: 'Too late' })).status, 409);
});

contract('a goal update voids an action awaiting confirmation without sending it', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
    ? { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Fixture tap needing confirmation', requiresConfirmation: true }
    : { type: 'done', message: 'Confirmation voided' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'awaiting_confirmation'; });
  const actionId = state.task.confirmation.actionId;
  const updated = await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/instruction`, fixture.session.token, { instruction: 'Do not tap' });
  assert.equal(updated.status, 200, await updated.clone().text());
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.deepEqual(state.task.steps.map(step => step.state), ['cancelled']);
  assert.equal(state.task.confirmation, null);
  const stale = await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/confirm`, fixture.session.token, { actionId, approved: true });
  assert.equal(stale.status, 409);
  assert.equal((await nativeCounts(fixture)).taps, 0);
  const history = JSON.parse(fixture.requests[1].messages[1].content[0].text).recentHistory;
  assert.equal(history[0].state, 'cancelled');
});

contract('a done reply asking for confirmation is re-planned once instead of ending the task', async t => {
  const replies = [
    { type: 'done', message: '切换账号会更改账户设置。请确认是否继续退出当前账号？' },
    { type: 'done', message: 'Fixture verified after re-plan' },
  ];
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(replies[Math.min(call, replies.length) - 1])));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.message, 'Fixture verified after re-plan');
  assert.equal(fixture.requests.length, 2);
  assert.equal(state.task.steps.length, 0);
  const history = JSON.parse(fixture.requests[1].messages[1].content[0].text).recentHistory;
  assert.equal(history.at(-1).state, 'rejected');
  assert.match(history.at(-1).result.message, /requiresConfirmation:true/);
  assert.equal((await nativeCounts(fixture)).counts.screenshot, 1, 'the re-plan reuses the same observation');
});

contract('repeated actions that leave the screen unchanged are warned about and then stopped', async t => {
  // The fixture returns the same screenshot and elements every time, so no tap changes the screen.
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(
    { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: `Fixture tap ${call}` })),
  { FIXTURE_STATIC_ELEMENTS: 'Button: Fixture' });
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'failed', JSON.stringify(state.task));
  assert.match(state.task.error, /屏幕都没有变化/);
  assert.equal(state.task.steps.length, 8);
  const warned = fixture.requests.map(r => JSON.parse(r.messages[1].content[0].text).recentHistory)
    .find(history => history.some(entry => entry.state === 'warning'));
  assert.ok(warned, 'the model was warned before the task stopped');
  assert.ok(warned.filter(entry => entry.tool === 'iphone_tap').every(entry => entry.screenChanged === false));
});

contract('a new task in the same session sees the previous task as context', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(
    { type: 'done', message: call === 1 ? 'First fixture result' : 'Second fixture result' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'completed'; });
  const second = await fixture.app.api(fixture.route + '/tasks', fixture.session.token, { instruction: 'Continue the previous fixture task' });
  assert.equal(second.status, 201, await second.clone().text());
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'completed' && fixture.requests.length === 2; });
  const history = JSON.parse(fixture.requests[1].messages[1].content[0].text).recentHistory;
  assert.equal(history[0].previousTasks.length, 1);
  assert.equal(history[0].previousTasks[0].instruction, 'Inspect the fixture safely');
  assert.equal(history[0].previousTasks[0].result, 'First fixture result');
  assert.equal(JSON.parse(fixture.requests[0].messages[1].content[0].text).recentHistory.length, 0);
});

contract('task routes authenticate, check the origin and validate input before any provider call', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'Fixture verified' })));
  await until(async () => (await fixture.snapshot()).task?.state === 'completed');
  const { app, session: s, route } = fixture;
  const calls = fixture.requests.length;
  assert.equal((await app.api(route + '/tasks', 'wrong', { instruction: 'go' })).status, 401);
  assert.equal((await app.api(route + '/tasks', s.token, { instruction: 'go' }, { Origin: 'https://evil.test' })).status, 403);
  for (const body of [{ instruction: '' }, { instruction: 'x'.repeat(4001) }, { instruction: 'go', extra: true }, { instruction: 'go', autoConfirm: 'yes' }]) {
    assert.equal((await app.api(route + '/tasks', s.token, body)).status, 400, JSON.stringify(body).slice(0, 60));
  }
  assert.equal(fixture.requests.length, calls);
  const stop = `${route}/tasks/${fixture.task.id}/stop`;
  assert.equal((await app.api(stop, 'wrong', {})).status, 401);
  assert.equal((await app.api(stop, s.token, {}, { Origin: 'https://evil.test' })).status, 403);
  assert.equal((await app.api(stop, s.token, { extra: true })).status, 400);
  assert.equal((await app.api(`${route}/tasks/missing/stop`, s.token, {})).status, 404);
});

contract('sensitive actions wait for confirmation even when the model omits its flag', async t => {
  for (const action of [
    { tool: 'iphone_terminate_app', args: { app: 'com.apple.MobileSMS' }, reason: 'Close the app' },
    { tool: 'iphone_open_url', args: { url: 'sms:123' }, reason: 'Open the link' },
    { tool: 'iphone_tap_element', args: { text: '发送' }, reason: 'Tap the button' },
    { tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Send the message' },
  ]) {
    await t.test(action.tool, async t => {
      const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
        ? { type: 'action', ...action } : { type: 'done', message: 'Fixture verified' })));
      let state;
      await until(async () => { state = await fixture.snapshot(); return ['awaiting_confirmation', 'failed', 'completed'].includes(state.task?.state); });
      assert.equal(state.task.state, 'awaiting_confirmation', JSON.stringify(state.task));
      assert.equal(state.task.confirmation.tool, action.tool);
      const before = await nativeCounts(fixture);
      assert.equal(before.taps, 0); assert.equal(before.counts.terminate_app, 0);
      await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/confirm`, fixture.session.token, { actionId: state.task.confirmation.actionId, approved: true });
      await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
      assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
      assert.deepEqual(state.task.steps.map(step => step.state), ['succeeded']);
    });
  }
});

contract('confirmation decisions are authenticated and validated, and a second task cannot start while one waits', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
    ? { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Fixture tap', requiresConfirmation: true }
    : { type: 'done', message: 'Fixture verified' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'awaiting_confirmation'; });
  const { app, session: s, route } = fixture;
  assert.equal((await app.api(route + '/tasks', s.token, { instruction: 'another' })).status, 409);
  const url = `${route}/tasks/${fixture.task.id}/confirm`;
  const decision = { actionId: state.task.confirmation.actionId, approved: true };
  assert.equal((await app.api(url, 'wrong', decision)).status, 401);
  assert.equal((await app.api(url, s.token, decision, { Origin: 'https://evil.test' })).status, 403);
  assert.equal((await app.api(url, s.token, { ...decision, approved: 'yes' })).status, 400);
  assert.equal((await app.api(url, s.token, { ...decision, extra: true })).status, 400);
  assert.equal((await nativeCounts(fixture)).taps, 0);
  assert.equal((await app.api(url, s.token, decision)).status, 200);
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'completed'; });
  assert.equal((await nativeCounts(fixture)).taps, 1);
});

contract('declining or stopping a pending confirmation cancels the action without dispatch', async t => {
  for (const choice of ['decline', 'stop']) {
    await t.test(choice, async t => {
      const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
        type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Fixture tap', requiresConfirmation: true,
      })));
      let state;
      await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'awaiting_confirmation'; });
      const url = `${fixture.route}/tasks/${fixture.task.id}`;
      const { actionId } = state.task.confirmation;
      if (choice === 'decline') assert.equal((await fixture.app.api(url + '/confirm', fixture.session.token, { actionId, approved: false })).status, 200);
      else assert.equal((await fixture.app.api(url + '/stop', fixture.session.token, {})).status, 200);
      state = await fixture.snapshot();
      assert.equal(state.task.state, 'stopped');
      assert.deepEqual(state.task.steps.map(step => step.state), ['cancelled']);
      assert.equal(state.task.confirmation, null);
      assert.equal((await fixture.app.api(url + '/confirm', fixture.session.token, { actionId, approved: true })).status, 409);
      await pause(50);
      assert.equal((await nativeCounts(fixture)).taps, 0);
      assert.equal(fixture.requests.length, 1);
    });
  }
});

contract('stopping during planning discards the late model reply', async t => {
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  t.after(() => release());
  const fixture = await modelReplyTask(t, async () => {
    await gate;
    return modelCompletion(JSON.stringify({ type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Late fixture tap' }));
  });
  await until(() => fixture.requests.length === 1);
  assert.equal((await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/stop`, fixture.session.token, {})).status, 200);
  release();
  await pause(100);
  const state = await fixture.snapshot();
  assert.equal(state.task.state, 'stopped');
  assert.equal(state.task.steps.length, 0);
  assert.equal((await nativeCounts(fixture)).taps, 0);
});

contract('stopping while an action executes reports it as uncertain and never retries it', async t => {
  // The native fixture accepts a tap at x=999 but never answers it.
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_tap', args: { x: 999, y: 7 }, reason: 'Fixture tap that never returns',
  })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'executing'; });
  await until(async () => (await nativeCounts(fixture)).taps === 1);
  assert.equal((await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/stop`, fixture.session.token, {})).status, 200);
  state = await fixture.snapshot();
  assert.equal(state.task.state, 'stopped');
  assert.equal(state.task.steps[0].state, 'uncertain');
  assert.match(state.task.message, /cannot be undone/i);
  await pause(100);
  assert.equal((await nativeCounts(fixture)).taps, 1);
  assert.equal(fixture.requests.length, 1);
});
