import assert from 'node:assert/strict';
import { fixtureImageBase64 } from './fixtures/screen-image.mjs';
import http from 'node:http';
import {
  contract, device, pause, until, harness, peer, ready, modelReplyTask, modelCompletion, nativeCounts,
} from './helpers/rust-gateway.mjs';

// Model provider protocol: reply decoding and correction, schema fallback, catalog and observation modes.

contract('model reply accepts complete JSON fences and text content parts', async t => {
  const done = JSON.stringify({ type: 'done', message: 'Fixture verified' });
  for (const [name, content] of [
    ['JSON fence', '```json\n' + done + '\n```'],
    ['text parts', [{ type: 'text', text: done.slice(0, 15) }, { type: 'text', text: done.slice(15) }]],
  ]) {
    await t.test(name, async t => {
      const fixture = await modelReplyTask(t, () => modelCompletion(content));
      let state;
      await until(async () => {
        state = await fixture.snapshot();
        return ['completed', 'failed'].includes(state.task?.state);
      });
      assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
      assert.equal(fixture.requests.length, 1);
    });
  }
});

contract('model reply honors optional null parameters accepted by the native command validator', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
    ? { type: 'action', tool: 'iphone_swipe', args: { from_x: 1, from_y: 2, to_x: 3, to_y: 4, seconds: null }, reason: 'Fixture swipe' }
    : { type: 'done', message: 'Fixture verified' })));
  let state;
  await until(async () => {
    state = await fixture.snapshot();
    return ['completed', 'failed'].includes(state.task?.state);
  });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.steps.length, 1);
  assert.equal(state.task.steps[0].state, 'succeeded');
});

contract('model reply requests a schema requiring a non-null action tool from the provider', async t => {
  const fixture = await modelReplyTask(t, (_call, request) => modelCompletion(JSON.stringify(
    request.response_format?.type === 'json_schema'
      ? { type: 'done', message: 'Structured fixture verified' }
      : { type: 'action', tool: null, args: {}, reason: 'Malformed fixture action' }
  )));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  const format = fixture.requests[0].response_format;
  const action = format.json_schema.schema.anyOf.find(branch => branch.properties.type.enum.includes('action'));
  assert.equal(format.json_schema.name, 'phone_reply');
  assert.equal(format.json_schema.strict, false);
  assert.ok(action.required.includes('tool'));
  assert.equal(action.properties.tool.type, 'string');
  assert.equal(action.additionalProperties, false);
  assert.equal(fixture.requests.length, 1);
  assert.equal(state.task.steps.length, 0);
});

contract('model reply corrects missing and non-text tools with the same schema before dispatch', async t => {
  for (const [name, fields] of [['missing', {}], ['null', { tool: null }], ['object', { tool: { name: 'iphone_tap' } }]]) {
    await t.test(name, async t => {
      const fixture = await modelReplyTask(t, (call, request) => modelCompletion(JSON.stringify(
        call === 1 || !request.response_format
          ? { type: 'action', ...fields, args: {}, reason: 'Malformed fixture action' }
          : { type: 'done', message: 'No action needed' }
      )));
      let state;
      await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
      assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
      assert.equal(fixture.requests.length, 2);
      assert.deepEqual(fixture.requests[1].response_format, fixture.requests[0].response_format);
      // The correction must not suggest a concrete tool that the model might copy.
      const correction = fixture.requests[1].messages.at(-1).content;
      assert.match(correction, /string tool name from the catalog/);
      assert.doesNotMatch(correction, /iphone_/);
      assert.equal(state.task.steps.length, 0);
    });
  }
});

contract('model reply schema never bypasses native validation and correction cannot replay earlier actions', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(JSON.stringify(call === 1
    ? { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Fixture action' }
    : call === 2
      ? { type: 'action', tool: null, args: {}, reason: 'Malformed fixture action' }
      : { type: 'done', message: 'Fixture verified' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.steps.length, 1);
  assert.equal(fixture.requests.length, 3);
  assert.equal((await nativeCounts(fixture)).taps, 1);
});

contract('model reply schema refusal falls back to schema instructions and remembers the model capability', async t => {
  const fixture = await modelReplyTask(t, call => call === 1
    ? { status: 400, body: { error: { message: 'Unsupported response_format json_schema' } } }
    : modelCompletion(JSON.stringify({ type: 'done', message: 'Fixture verified' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(fixture.requests.length, 2);
  assert.equal(fixture.requests[1].response_format, undefined);
  assert.match(fixture.requests[1].messages[0].content, /"required":\["type","tool","args","reason"\]/);
  await fixture.app.api(fixture.route + '/tasks', fixture.session.token, { instruction: 'Inspect the fixture again' });
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'completed' && fixture.requests.length === 3; });
  assert.equal(fixture.requests[2].response_format, undefined);
  assert.equal(state.task.steps.length, 0);
});

contract('model reply schema fallback is bounded and unrelated provider errors remain fatal', async t => {
  for (const [name, first, calls, status] of [
    ['unrelated 400', { status: 400, body: { error: 'private invalid argument' } }, 1, 400],
    ['unrelated 422', { status: 422, body: { error: 'private invalid request' } }, 1, 422],
    ['refused format then HTTP failure', { status: 422, body: { error: 'json_schema unsupported' } }, 2, 403],
  ]) {
    await t.test(name, async t => {
      const fixture = await modelReplyTask(t, call => call === 1 ? first : { status: 403, body: { error: 'private HTTP error' } });
      let state;
      await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
      assert.match(state.task.error, new RegExp(`HTTP ${status}`));
      assert.equal(fixture.requests.length, calls);
      assert.equal(state.task.steps.length, 0);
      assert.ok(!JSON.stringify(state.task).includes('private'));
    });
  }
});

contract('model reply correction preserves confirmation and executes the corrected action once', async t => {
  const fixture = await modelReplyTask(t, call => modelCompletion(call === 1
    ? 'I will inspect the fixture next.'
    : JSON.stringify(call === 2
      ? { type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Fixture action', requiresConfirmation: true }
      : { type: 'done', message: 'Fixture verified' })));
  let state;
  await until(async () => {
    state = await fixture.snapshot();
    return state.task?.state === 'awaiting_confirmation' || state.task?.state === 'failed';
  });
  assert.equal(state.task.state, 'awaiting_confirmation', JSON.stringify(state.task));
  assert.equal(fixture.requests.length, 2);
  assert.deepEqual(fixture.requests[1].messages.slice(0, 2), fixture.requests[0].messages);
  assert.match(fixture.requests[1].messages.at(-1).content, /JSON/);
  const command = { requestId: 'before-confirmation', deviceId: fixture.session.deviceId, tool: 'iphone_status' };
  const before = await (await fixture.app.api(fixture.route + '/command', fixture.session.token, command)).json();
  assert.equal(JSON.parse(before.text).taps, 0);
  await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/confirm`, fixture.session.token, {
    actionId: state.task.confirmation.actionId, approved: true,
  });
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'completed'; });
  const after = await (await fixture.app.api(fixture.route + '/command', fixture.session.token, {
    ...command, requestId: 'after-confirmation',
  })).json();
  assert.equal(JSON.parse(after.text).taps, 1);
  assert.equal(state.task.steps.length, 1);
  assert.equal(fixture.requests.length, 3);
});

contract('model reply correction is bounded and reports the invalid argument without raw reply text', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({
    type: 'action', tool: 'iphone_tap', args: { x: 'private-invalid-value', y: 7 }, reason: 'Fixture action',
  })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
  assert.equal(fixture.requests.length, 2);
  assert.equal(state.task.steps.length, 0);
  assert.match(state.task.error, /args.*x|Invalid x/);
  assert.ok(!JSON.stringify(state.task).includes('private-invalid-value'));
});

contract('model reply marked length is corrected before any device action even when its JSON parses', async t => {
  const fixture = await modelReplyTask(t, call => call === 1
    ? modelCompletion(JSON.stringify({ type: 'action', tool: 'iphone_tap', args: { x: 5, y: 7 }, reason: 'Truncated response' }), 'length')
    : modelCompletion(JSON.stringify({ type: 'done', message: 'No action was required' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(state.task.steps.length, 0);
  assert.equal(fixture.requests.length, 2);
  assert.ok(fixture.requests[1].max_tokens > fixture.requests[0].max_tokens);
});

contract('model reply recovery stops on provider failures and explicit refusals', async t => {
  for (const [name, response, error] of [
    ['HTTP failure', { status: 403, body: { error: 'private-provider-error' } }, /HTTP 403/],
    ['refusal', { body: { choices: [{ message: { content: null, refusal: 'private refusal text' } }] } }, /refus|拒绝/i],
  ]) {
    await t.test(name, async t => {
      const fixture = await modelReplyTask(t, () => response);
      let state;
      await until(async () => { state = await fixture.snapshot(); return state.task?.state === 'failed'; });
      assert.match(state.task.error, error);
      assert.equal(fixture.requests.length, 1);
      assert.equal(state.task.steps.length, 0);
      assert.ok(!JSON.stringify(state.task).includes('private'));
    });
  }
});

contract('model reply correction is cancelled when the user stops the task', async t => {
  const fixture = await modelReplyTask(t, call => call === 1 ? modelCompletion('not JSON') : undefined);
  await until(async () => fixture.requests.length === 2 || (await fixture.snapshot()).task?.state === 'failed');
  assert.equal(fixture.requests.length, 2);
  await fixture.app.api(`${fixture.route}/tasks/${fixture.task.id}/stop`, fixture.session.token, {});
  const state = await fixture.snapshot();
  assert.equal(state.task.state, 'stopped');
  assert.equal(state.task.steps.length, 0);
});

contract('provider model catalog is cached and each task keeps its own validated model selection', async t => {
  let catalogCalls = 0; const generated = [];
  const provider = http.createServer(async (req, res) => {
    assert.equal(req.headers.authorization, 'Bearer fixture-catalog-key');
    res.setHeader('Content-Type', 'application/json');
    if (req.url === '/v1/models') {
      catalogCalls++;
      return res.end(JSON.stringify({ data: [
        { id: 'codex/default' }, { id: 'other/vision' }, { id: 'other/vision' },
        { id: 'other/text' }, { id: '' }, { id: 'bad\nmodel' }, { id: 'x'.repeat(257) },
      ] }));
    }
    assert.equal(req.url, '/v1/chat/completions');
    let body = ''; for await (const chunk of req) body += chunk;
    const input = JSON.parse(body); generated.push(input.model);
    await pause(50);
    res.end(JSON.stringify({ choices: [{ message: { content: JSON.stringify({ type: 'done', message: 'fixture completed' }) } }] }));
  });
  await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
  t.after(() => { provider.closeAllConnections(); provider.close(); });
  const app = await harness(t, { LLM_BASE_URL: `http://127.0.0.1:${provider.address().port}/v1`, LLM_MODEL: 'codex/default', LLM_API_KEY: 'fixture-catalog-key' });
  const configs = await Promise.all(Array.from({ length: 6 }, async () => (await app.api('/api/config')).json()));
  for (const config of configs) assert.deepEqual(config, { backend: 'openai', aiReady: true, model: 'codex/default', models: ['codex/default', 'other/text', 'other/vision'] });
  assert.equal(catalogCalls, 1); assert.ok(!JSON.stringify(configs).includes('fixture-catalog-key'));
  const first = await app.session(); await peer(t, first); await ready(app, first);
  const second = await app.session({ device: { ...device, serialNumber: '00008110-001234567890001F' } });
  await peer(t, second, { ...device, serialNumber: second.deviceId }); await ready(app, second);
  for (const model of ['not-listed', null, 42, '', 'bad\nmodel']) {
    const response = await app.api(`/api/sessions/${first.id}/tasks`, first.token, { instruction: 'observe fixture', model });
    assert.equal(response.status, 400);
  }
  const started = await app.api(`/api/sessions/${first.id}/tasks`, first.token, { instruction: 'observe fixture', model: 'other/vision' });
  assert.equal(started.status, 201); assert.equal((await started.json()).model, 'other/vision');
  assert.equal((await app.api(`/api/sessions/${second.id}/tasks`, second.token, { instruction: 'observe fixture' })).status, 201);
  await until(async () => (await (await app.api(`/api/sessions/${first.id}`, first.token)).json()).task?.state === 'completed');
  await until(async () => (await (await app.api(`/api/sessions/${second.id}`, second.token)).json()).task?.state === 'completed');
  assert.deepEqual(generated.sort(), ['codex/default', 'other/vision']);
  assert.equal((await (await app.api('/api/config')).json()).model, 'codex/default');
});

contract('failed or redirected model discovery preserves default use without leaking provider errors or credentials', async t => {
  let catalogs = 0, redirected = 0;
  const sink = http.createServer((_req, res) => { redirected++; res.end('{}'); });
  await new Promise(resolve => sink.listen(0, '127.0.0.1', resolve));
  const provider = http.createServer((req, res) => {
    if (req.url.endsWith('/models')) {
      catalogs++; res.writeHead(302, { Location: `http://127.0.0.1:${sink.address().port}/secret` });
      return res.end('private provider error and fixture-secret');
    }
    res.end(JSON.stringify({ choices: [{ message: { content: JSON.stringify({ type: 'done', message: 'fixture completed' }) } }] }));
  });
  await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
  t.after(() => { provider.closeAllConnections(); provider.close(); sink.closeAllConnections(); sink.close(); });
  const app = await harness(t, { LLM_BASE_URL: `http://127.0.0.1:${provider.address().port}/v1`, LLM_MODEL: 'default', LLM_API_KEY: 'fixture-secret' });
  for (let i = 0; i < 2; i++) {
    const config = await (await app.api('/api/config')).json();
    assert.deepEqual(config.models, ['default']); assert.ok(config.modelsError);
    assert.ok(!JSON.stringify(config).includes('fixture-secret')); assert.ok(!JSON.stringify(config).includes('private provider'));
  }
  assert.equal(catalogs, 1); assert.equal(redirected, 0);
  const s = await app.session(); await peer(t, s); await ready(app, s);
  const started = await app.api(`/api/sessions/${s.id}/tasks`, s.token, { instruction: 'observe fixture', model: 'default' });
  assert.equal(started.status, 201);
  await until(async () => (await (await app.api(`/api/sessions/${s.id}`, s.token)).json()).task?.state === 'completed');
});

contract('OCR-only planning skips automatic WDA elements and retains text geometry', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'OCR verified' })));
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal((await nativeCounts(fixture)).counts.list_elements, 0);
  const payload = fixture.requests[0];
  const input = JSON.parse(payload.messages[1].content[0].text);
  assert.equal(input.observation.ocr.width, 390);
  assert.equal(input.observation.ocr.height, 844);
  assert.deepEqual(input.observation.ocr.lines[0].phonePolygon, [[0, 0], [390, 0], [390, 844], [0, 844]]);
  assert.equal(input.observation.elementsText, undefined);
  assert.ok(!JSON.stringify(payload).includes(fixtureImageBase64));
  assert.ok(!JSON.stringify(payload).includes('image_url'));
});

contract('OCR failure stops the task before the model receives any request', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'Unexpected' })), {
    FIXTURE_OCR_ERROR: '1',
  });
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'failed', JSON.stringify(state.task));
  assert.match(state.task.error, /OCR/i);
  assert.equal(fixture.requests.length, 0);
});

contract('default image planning sends a screenshot without starting OCR', async t => {
  const fixture = await modelReplyTask(t, () => modelCompletion(JSON.stringify({ type: 'done', message: 'Image verified' })), {
    AI_OBSERVATION: '', FIXTURE_OCR_ERROR: '1',
  });
  let state;
  await until(async () => { state = await fixture.snapshot(); return ['completed', 'failed'].includes(state.task?.state); });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(fixture.requests.length, 1);
  const content = fixture.requests[0].messages[1].content;
  assert.equal(content.filter(part => part.type === 'image_url').length, 1);
  assert.equal(JSON.parse(content[0].text).observation.ocr, undefined);
});

async function provider(t, handler) {
  const server = http.createServer(handler);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  return `http://127.0.0.1:${server.address().port}`;
}
async function runTask(t, base, env = {}) {
  const app = await harness(t, { LLM_BASE_URL: `${base}/v1`, LLM_MODEL: 'fixture-vision', LLM_API_KEY: 'server-only-key', ...env });
  const s = await app.session(); await peer(t, s); await ready(app, s);
  const route = `/api/sessions/${s.id}`;
  assert.equal((await app.api(route + '/tasks', s.token, { instruction: 'Inspect the fixture' })).status, 201);
  let state;
  await until(async () => { state = await (await app.api(route, s.token)).json(); return ['completed', 'failed'].includes(state.task?.state); });
  return { app, state };
}

contract('image planning sends the screenshot and element text as untrusted evidence with the server key and no tools', async t => {
  const seen = [];
  const base = await provider(t, async (req, res) => {
    let body = ''; for await (const chunk of req) body += chunk;
    seen.push({ url: req.url, authorization: req.headers.authorization, body: JSON.parse(body) });
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ choices: [{ message: { content: JSON.stringify({ type: 'done', message: 'Fixture verified' }) } }] }));
  });
  const { state } = await runTask(t, base, { AI_OBSERVATION: 'image' });
  assert.equal(state.task.state, 'completed', JSON.stringify(state.task));
  assert.equal(seen.length, 1);
  const [call] = seen;
  assert.equal(call.url, '/v1/chat/completions');
  assert.equal(call.authorization, 'Bearer server-only-key');
  assert.equal(call.body.model, 'fixture-vision');
  assert.equal(call.body.tools, undefined);
  assert.match(call.body.messages[0].content, /untrusted/i);
  assert.match(call.body.messages[0].content, /iphone_swipe/);
  const content = call.body.messages[1].content;
  assert.deepEqual(content.find(part => part.type === 'image_url'), { type: 'image_url', image_url: { url: `data:image/jpeg;base64,${fixtureImageBase64}` } });
  assert.match(JSON.parse(content[0].text).observation.elementsText, /"op":"observe"/);
  assert.ok(!JSON.stringify(state).includes('server-only-key'));
});

contract('provider redirects are not followed and oversized replies are refused without leaking the key', async t => {
  let targetCalls = 0;
  const target = await provider(t, (_req, res) => { targetCalls++; res.end('unexpected redirect target'); });
  const redirecting = await provider(t, (req, res) => {
    req.resume(); res.writeHead(302, { Location: `${target}/stolen` }); res.end('server-only-key');
  });
  const redirected = await runTask(t, redirecting);
  assert.equal(redirected.state.task.state, 'failed');
  assert.equal(targetCalls, 0);
  assert.ok(!JSON.stringify(redirected.state).includes('server-only-key'));
  const oversized = await provider(t, (req, res) => {
    req.resume(); res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ choices: [{ message: { content: 'x'.repeat(70000) } }] }));
  });
  const large = await runTask(t, oversized);
  assert.equal(large.state.task.state, 'failed');
  assert.match(large.state.task.error, /too large/i);
  assert.equal(large.state.task.steps.length, 0);
});
