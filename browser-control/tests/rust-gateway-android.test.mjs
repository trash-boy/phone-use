import assert from 'node:assert/strict';
import { fixtureImageBase64 } from './fixtures/screen-image.mjs';
import { once } from 'node:events';
import http from 'node:http';
import WebSocket from 'ws';
import { encodeFrame, jsonPayload } from './support/relay-protocol.mjs';
import { contract, origin, token, until, harness, ready } from './helpers/rust-gateway.mjs';

// Android sessions driven through the browser ADB relay.

contract('Android uses browser ADB RPC with session isolation, compressed observations, deduplication and disconnect cleanup', async t => {
  const { AndroidRelay } = await import('../web/android/relay.mjs');
  const { AndroidCommands } = await import('../web/android/core.mjs');
  const app = await harness(t);
  const android = { platform:'android', serialNumber:'R5C_ABC-123',vendorId:1256,productId:1234,productName:'Android fixture' };
  const s = await app.session({ device: android });
  assert.equal(s.deviceId,'android:R5C_ABC-123'); assert.match(s.relayUrl,/\/device\/ws\?/);
  const duplicate = await app.api('/api/sessions',token,{device:android}); assert.equal(duplicate.status,409);
  const calls=[];
  const commands = new AndroidCommands({deviceId:s.deviceId,delay:async()=>{},
    shell:async command=>{calls.push(command);if(command.startsWith('uiautomator'))return '<hierarchy><node text="设置" enabled="true" bounds="[0,0][100,100]"/></hierarchy>';return '';},
    capture:async()=>({imageBase64:'anBlZw==',caption:'Android full screen 576x1280',view:{width:576,height:1280,nativeWidth:1080,nativeHeight:2400},sourceBytes:1000000,uploadedBytes:90000}),
    controller:{setClipboard:async value=>calls.push(value.content)},
  });
  const ws = new WebSocket(s.relayUrl,{origin});
  const relay = new AndroidRelay(ws,s.deviceId,(request,signal)=>commands.execute(request,signal));
  t.after(()=>relay.close());
  await once(ws,'open');
  ws.send(encodeFrame({type:1,payload:jsonPayload({...android,serialNumber:s.deviceId})}));await relay.ready;
  const state = await ready(app,s);assert.equal(state.platform,'android');assert.equal(state.daemon.state.state,'ready');assert.equal(state.setup.state,'complete');
  const route=`/api/sessions/${s.id}`;
  const screen=await (await app.api(route+'/screen',s.token)).json();
  assert.equal(screen.imageBase64,'anBlZw==');assert.match(screen.elementsText,/设置/);assert.equal(screen.deviceId,s.deviceId);
  const body={requestId:'unicode',deviceId:s.deviceId,tool:'iphone_type_text',args:{text:'你好安卓',submit:false}};
  const response=await app.api(route+'/command',s.token,body);assert.equal(response.status,200);assert.equal((await response.json()).kind,'text');
  assert.equal((await app.api(route+'/command',s.token,body)).status,200);assert.equal(calls.filter(c=>c==='你好安卓').length,1,'duplicate HTTP request must not type twice');
  assert.equal((await app.api(route+'/command',s.token,{...body,requestId:'wrong-device',deviceId:'android:OTHER'})).status,409);
  assert.equal((await app.api(route+'/command',token,body)).status,401,'controller token cannot control a session');
  const disconnected=once(ws,'close');relay.close();await disconnected;
  await until(async()=>[404,410].includes((await app.api(route,s.token)).status));
  const replacement=await app.session({device:android});assert.notEqual(replacement.id,s.id);
});

contract('Android AI tasks observe the Android device and require confirmation before Unicode submit', async t => {
  const { AndroidRelay } = await import('../web/android/relay.mjs');
  const { AndroidCommands } = await import('../web/android/core.mjs');
  const bodies=[];
  const provider=http.createServer(async (request,response)=>{
    let body='';for await(const chunk of request) body+=chunk;
    bodies.push(JSON.parse(body));
    const action=bodies.length===1 ? {type:'action',tool:'iphone_type_text',args:{text:'你好，安卓 👋',submit:true},reason:'输入用户指定的中文并提交'} : {type:'done',message:'已观察提交后的 Android 页面'};
    response.setHeader('Content-Type','application/json');response.end(JSON.stringify({choices:[{message:{content:JSON.stringify(action)}}]}));
  });
  await new Promise(resolve=>provider.listen(0,'127.0.0.1',resolve));
  t.after(()=>{provider.closeAllConnections();provider.close();});
  const app=await harness(t,{LLM_BASE_URL:`http://127.0.0.1:${provider.address().port}/v1`,LLM_MODEL:'fixture',LLM_API_KEY:'fixture-only'});
  const selected={platform:'android',serialNumber:'AI_TEST',vendorId:1256,productId:1234};
  const s=await app.session({device:selected});
  const typed=[],keys=[];let captures=0;
  const commands=new AndroidCommands({deviceId:s.deviceId,delay:async()=>{},
    controller:{setClipboard:async message=>typed.push(message.content)},
    shell:async command=>{if(command.startsWith('input keyevent')) keys.push(command);return '<hierarchy><node text="结果" bounds="[0,0][100,100]"/></hierarchy>';},
    capture:async()=>{captures++;return {imageBase64:fixtureImageBase64,caption:'Android full screen 576x1280',view:{width:576,height:1280,nativeWidth:1080,nativeHeight:2400},sourceBytes:1000000,uploadedBytes:90000};},
  });
  const ws=new WebSocket(s.relayUrl,{origin});
  const relay=new AndroidRelay(ws,s.deviceId,(request,signal)=>commands.execute(request,signal));
  t.after(()=>relay.close());await once(ws,'open');
  ws.send(encodeFrame({type:1,payload:jsonPayload(selected)}));await relay.ready;await ready(app,s);
  const route=`/api/sessions/${s.id}`;
  assert.equal((await app.api(route+'/tasks',s.token,{instruction:'输入你好，安卓并提交'})).status,201);
  let state;
  await until(async()=>{state=await(await app.api(route,s.token)).json();return state.task?.state==='awaiting_confirmation';});
  assert.deepEqual(typed,[]);assert.deepEqual(keys,[]);
  const task=state.task;
  assert.equal((await app.api(route+`/tasks/${task.id}/confirm`,s.token,{actionId:task.confirmation.actionId,approved:true})).status,200);
  await until(async()=>{state=await(await app.api(route,s.token)).json();return ['completed','failed'].includes(state.task?.state);});
  assert.equal(state.task.state,'completed',JSON.stringify(state.task));
  assert.deepEqual(typed,['你好，安卓 👋']);assert.deepEqual(keys,['input keyevent 66']);assert.equal(captures,2);
  assert.equal(bodies.length,2);assert.match(JSON.parse(bodies[0].messages[1].content[0].text).observation.caption,/Android/);
  assert.equal(state.deviceId,'android:AI_TEST');assert.equal(state.setup.state,'complete');
});
