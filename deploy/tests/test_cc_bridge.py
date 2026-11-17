"""Stdlib-only tests; subprocess fixtures never invoke the authenticated CLI."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import stat
import tempfile
import unittest
from unittest import mock

BRIDGE_PATH = Path(__file__).resolve().parents[2] / "browser-control" / "cc" / "bridge.py"
spec = importlib.util.spec_from_file_location("cc_bridge", BRIDGE_PATH)
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)

PLAN = {"model": "sonnet", "system": "Return a JSON object", "content": [{"type": "text", "text": "hello"}]}

FAKE = '''#!/usr/bin/env python3
import json, os, subprocess, sys, time
args = sys.argv[1:]
model = args[args.index('--model') + 1]
if model in ('sleep', 'descendant'):
    if model == 'descendant':
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
        with open('child.pid', 'w') as f:
            f.write(str(child.pid))
    with open('leader.pid', 'w') as f:
        f.write(str(os.getpid()))
    time.sleep(60)
if model == 'overflow':
    sys.stdout.write('x' * (1024 * 1024 + 1))
    sys.exit(0)
if model == 'stderr-overflow':
    sys.stderr.write('SECRET' * 200000)
    sys.exit(1)
if model == 'failure':
    sys.stderr.write('SECRET AUTH PAYLOAD')
    sys.exit(1)
message = json.loads(sys.stdin.readline())
system_file = args[args.index('--system-prompt-file') + 1] if '--system-prompt-file' in args else None
system = open(system_file).read() if system_file else args[args.index('--system-prompt') + 1]
reply = {'args': args, 'message': message, 'cwd': os.getcwd(), 'pid': os.getpid(),
         'nested': [os.environ.get('CLAUDECODE'), os.environ.get('CLAUDE_CODE_ENTRYPOINT')],
         'auth': os.environ.get('BRIDGE_TEST_AUTH'), 'system_file': system_file, 'system_length': len(system)}
print(json.dumps({'type': 'result', 'result': json.dumps(reply)}))
'''


class ParsingTests(unittest.TestCase):
    def test_text_and_image_translation(self):
        plan = dict(PLAN, content=[{"type": "text", "text": "look"},
                                   {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk=", "detail": "high"}}])
        model, system, blocks = bridge.translate_plan(json.dumps(plan).encode())
        self.assertEqual((model, system), (PLAN["model"], PLAN["system"]))
        self.assertEqual(blocks[1], {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}})

    def test_optional_plan_fields(self):
        image = {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
        plan = bridge.load_plan(json.dumps(PLAN).encode())
        self.assertEqual((plan.deadline_ms, plan.follow_up, plan.session, plan.images), (None, None, None, [None]))
        plan = bridge.load_plan(json.dumps(dict(PLAN, deadlineMs=5000, session="s1", content=[image],
                                                followUp=[{"type": "text", "text": "delta"}, image])).encode())
        self.assertEqual((plan.deadline_ms, plan.session), (5000, "s1"))
        self.assertEqual(plan.images, [b"hi"], "images are decoded once, at validation")
        self.assertEqual(plan.follow_up[1], [None, b"hi"])
        for fields in ({"deadlineMs": "5"}, {"deadlineMs": True}, {"deadlineMs": -1}, {"followUp": []},
                       {"followUp": [{"type": "image_url", "image_url": {"url": "x"}}]}, {"session": "a b"}):
            with self.subTest(fields=fields), self.assertRaises(bridge.BridgeError) as error:
                bridge.load_plan(json.dumps(dict(PLAN, **fields)).encode())
            self.assertEqual(error.exception.status, 400)

    def test_session_close_body(self):
        self.assertEqual(bridge.closing_session(b'{"session":"s1"}'), "s1")
        for body in (b"", b"[]", b"{}", b'{"session":5}', b'{"session":"a b"}'):
            with self.subTest(body=body), self.assertRaises(bridge.BridgeError) as error:
                bridge.closing_session(body)
            self.assertEqual(error.exception.status, 400)

    def test_reject_invalid_plans(self):
        invalid = [b"no json", b"[]", json.dumps(dict(PLAN, model="")).encode(),
                   json.dumps(dict(PLAN, system=None)).encode(), json.dumps(dict(PLAN, content=[])).encode()]
        for url in ("https://example.com/image.png", "data:image/svg+xml;base64,aGk=", "data:image/png;base64,a", "data:image/png;base64,"):
            invalid.append(json.dumps(dict(PLAN, content=[{"type": "image_url", "image_url": {"url": url}}])).encode())
        for body in invalid:
            with self.subTest(body=body), self.assertRaises(bridge.BridgeError) as error:
                bridge.translate_plan(body)
            self.assertEqual(error.exception.status, 400)

    def test_cli_response_json_and_fences(self):
        for text in ('{"x":1}', '```json\n{"x":1}\n```', '```\n{"x":1}\n```'):
            self.assertEqual(bridge.parse_result(json.dumps({"type": "system"}) + "\n" + json.dumps({"type": "result", "result": text})), {"reply": {"x": 1}})
        for envelope in ({"type": "result", "result": "[]"}, {"type": "result", "result": "secret invalid"}, {"type": "result", "result": "{}", "is_error": True}, {"type": "result", "subtype": "error_max_turns", "result": "{}"}, [], {}):
            with self.assertRaises(bridge.BridgeError) as error:
                bridge.parse_result(json.dumps(envelope))
            self.assertNotIn("secret", str(error.exception))

    def test_socket_path_guards(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bridge.sock"
            path.write_text("keep")
            with self.assertRaises(ValueError):
                bridge.prepare_socket(path)
            self.assertEqual(path.read_text(), "keep")
            path.unlink()
            path.symlink_to(Path(directory) / "missing")
            with self.assertRaises(ValueError):
                bridge.prepare_socket(path)
            path.unlink()
            listener = socket.socket(socket.AF_UNIX)
            self.addCleanup(listener.close)
            listener.bind(str(path))
            listener.listen()
            with self.assertRaises(ValueError):
                bridge.prepare_socket(path)
            self.assertTrue(path.exists())
            listener.close()
            bridge.prepare_socket(path)
            self.assertFalse(path.exists())


class RequestTests(unittest.IsolatedAsyncioTestCase):
    async def parse(self, data):
        reader = asyncio.StreamReader(limit=bridge.MAX_HEADERS)
        reader.feed_data(data)
        reader.feed_eof()
        return await bridge.read_request(reader)

    async def test_valid_request(self):
        self.assertEqual(await self.parse(b"POST /plan HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}"), ("POST", "/plan", b"{}"))

    async def test_invalid_framing_and_limits(self):
        cases = [(b"POST /plan HTTP/1.1\r\n\r\n", 411),
                 (b"POST /plan HTTP/1.1\r\nContent-Length: 16777217\r\n\r\n", 413),
                 (b"POST /plan HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}", 400),
                 (b"POST /plan HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
                 (b"POST /plan HTTP/1.1\r\nContent-Length: -1\r\n\r\n", 400),
                 (b"POST /plan HTTP/1.1\r\nContent-Length: 5\r\n\r\n{}", 400),
                 (b"GET /health HTTP/1.1\r\nX: " + b"x" * bridge.MAX_HEADERS + b"\r\n\r\n", 431)]
        for raw, status in cases:
            with self.subTest(status=status), self.assertRaises(bridge.BridgeError) as error:
                await self.parse(raw)
            self.assertEqual(error.exception.status, status)


class IntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.executable = self.root / "fake-claude"
        self.executable.write_text(FAKE)
        self.executable.chmod(0o700)
        self.work = self.root / "work"
        self.work.mkdir()
        self.path = self.root / "bridge.sock"
        self.bridge = bridge.Bridge(str(self.executable), self.work, timeout=3)
        self.server = await asyncio.start_unix_server(self.bridge.handle, str(self.path), limit=bridge.MAX_HEADERS)
        self.writers = []

    async def asyncTearDown(self):
        for writer in self.writers:
            writer.close()
            await writer.wait_closed()
        self.server.close()
        await self.server.wait_closed()
        pending = list(self.bridge.handlers)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        await self.bridge.close()

    async def connect(self, model="sonnet", method="POST", target="/plan", **fields):
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        self.writers.append(writer)
        body = json.dumps(dict(PLAN, model=model, **fields)).encode() if method == "POST" else b""
        writer.write(f"{method} {target} HTTP/1.1\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
        await writer.drain()
        return reader, writer

    async def response(self, reader):
        raw = await asyncio.wait_for(reader.read(), 5)
        header, body = raw.split(b"\r\n\r\n", 1)
        return int(header.split()[1]), json.loads(body)

    async def wait_file(self, name):
        path = self.work / name
        async with asyncio.timeout(3):
            while not path.exists():
                await asyncio.sleep(0.01)
        return int(path.read_text())

    async def wait_idle(self):
        async with asyncio.timeout(3):
            while self.bridge.active:
                await asyncio.sleep(0.01)

    async def test_cli_argv_stdin_env_and_workdir(self):
        with mock.patch.dict(os.environ, {"CLAUDECODE": "nested", "CLAUDE_CODE_ENTRYPOINT": "sdk", "BRIDGE_TEST_AUTH": "preserved"}):
            reader, _ = await self.connect()
            status, result = await self.response(reader)
        self.assertEqual(status, 200)
        reply = result["reply"]
        self.assertEqual(reply["nested"], [None, None])
        self.assertEqual(reply["auth"], "preserved")
        self.assertEqual(os.path.realpath(reply["cwd"]), os.path.realpath(self.work))
        self.assertEqual(reply["message"], {"type": "user", "message": {"role": "user", "content": PLAN["content"]}})
        args = reply["args"]
        for key, value in (("--tools", ""), ("--mcp-config", '{"mcpServers":{}}'),
                           ("--setting-sources", ""), ("--permission-mode", "dontAsk"),
                           ("--settings", '{"disableAllHooks":true}'), ("--system-prompt", PLAN["system"])):
            self.assertEqual(args[args.index(key) + 1], value)
        for flag in ("--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence", "--safe-mode"):
            self.assertIn(flag, args)

    async def wait_warm(self):
        async with asyncio.timeout(3):
            while self.bridge.warm is None:
                await asyncio.sleep(0.01)
        return self.bridge.warm[1]

    async def test_next_step_reuses_a_prestarted_cli(self):
        reader, _ = await self.connect()
        first = (await self.response(reader))[1]["reply"]["pid"]
        warm = await self.wait_warm()
        self.assertNotEqual(warm.pid, first)
        reader, _ = await self.connect()
        status, result = await self.response(reader)
        self.assertEqual(status, 200)
        self.assertEqual(result["reply"]["pid"], warm.pid)
        self.assertEqual(result["reply"]["message"]["message"]["content"], PLAN["content"])
        replacement = await self.wait_warm()
        self.assertNotEqual(replacement.pid, warm.pid)

    async def test_prestarted_cli_is_not_used_for_another_model(self):
        reader, _ = await self.connect()
        await self.response(reader)
        warm = await self.wait_warm()
        reader, _ = await self.connect(model="haiku")
        status, result = await self.response(reader)
        self.assertEqual(status, 200)
        self.assertNotEqual(result["reply"]["pid"], warm.pid)
        self.assertEqual(result["reply"]["args"][result["reply"]["args"].index("--model") + 1], "haiku")
        async with asyncio.timeout(3):
            await warm.wait()

    async def test_failed_plans_do_not_prestart(self):
        reader, _ = await self.connect(model="failure")
        self.assertEqual((await self.response(reader))[0], 502)
        await asyncio.sleep(0.2)
        self.assertIsNone(self.bridge.warm)

    async def test_idle_prestarted_cli_expires(self):
        self.bridge.warm_idle = 0.2
        reader, _ = await self.connect()
        await self.response(reader)
        warm = await self.wait_warm()
        async with asyncio.timeout(3):
            await warm.wait()
        self.assertIsNone(self.bridge.warm)

    async def test_health_and_not_found(self):
        reader, _ = await self.connect(method="GET", target="/health")
        self.assertEqual(await self.response(reader), (200, {"ok": True}))
        self.assertTrue(await asyncio.to_thread(bridge.check_health, str(self.path)))
        reader, _ = await self.connect(method="GET", target="/other")
        self.assertEqual((await self.response(reader))[0], 404)

    async def test_large_system_prompt_goes_through_a_removed_file(self):
        system = "rule " * (bridge.SYSTEM_ARG_MAX // 4)
        reader, _ = await self.connect(system=system)
        status, result = await self.response(reader)
        self.assertEqual(status, 200)
        reply = result["reply"]
        self.assertNotIn("--system-prompt", reply["args"])
        self.assertEqual(reply["system_length"], len(system))
        self.assertTrue(reply["system_file"].startswith(str(self.work)))
        self.assertFalse(Path(reply["system_file"]).exists(), "the file goes with its process")
        warm = await self.wait_warm()
        self.assertIsNone(warm.stderr, "an idle pre-started CLI must not block on an unread stderr pipe")
        await self.bridge.close()
        self.assertEqual(list(self.work.iterdir()), [])

    async def test_session_close_and_follow_up_are_accepted(self):
        reader, _ = await self.connect(followUp=[{"type": "text", "text": "delta"}], session="s1")
        status, result = await self.response(reader)
        self.assertEqual(status, 200)
        self.assertEqual(result["reply"]["message"]["message"]["content"], PLAN["content"], "Claude ignores followUp")
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        self.writers.append(writer)
        writer.write(b'POST /session/close HTTP/1.1\r\nContent-Length: 16\r\n\r\n{"session":"s1"}')
        self.assertEqual(await self.response(reader), (200, {"ok": True}))

    async def test_deadline_bounds_the_plan(self):
        self.bridge.timeout = 30
        started = asyncio.get_running_loop().time()
        reader, _ = await self.connect(model="sleep", deadlineMs=3500)
        self.assertEqual((await self.response(reader))[0], 504)
        self.assertLess(asyncio.get_running_loop().time() - started, 3)
        reader, _ = await self.connect(deadlineMs=2500)
        self.assertEqual(await self.response(reader), (504, {"error": "Planning deadline exceeded"}))

    async def test_output_limits_and_sanitized_failures(self):
        for model in ("overflow", "stderr-overflow", "failure"):
            reader, _ = await self.connect(model=model)
            status, body = await self.response(reader)
            self.assertEqual(status, 502)
            self.assertNotIn("SECRET", json.dumps(body))
        self.assertEqual(self.bridge.active, 0)

    async def test_timeout_kills_process(self):
        self.bridge.timeout = 2
        reader, _ = await self.connect(model="sleep")
        pid = await self.wait_file("leader.pid")
        self.assertEqual((await self.response(reader))[0], 504)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        self.assertEqual(self.bridge.active, 0)

    async def test_nonblocking_concurrency_limit(self):
        first, _ = await self.connect(model="sleep")
        second, _ = await self.connect(model="sleep")
        async with asyncio.timeout(3):
            while self.bridge.active != 2:
                await asyncio.sleep(0.01)
        third, _ = await self.connect()
        self.assertEqual((await self.response(third))[0], 503)
        health, _ = await self.connect(method="GET", target="/health")
        self.assertEqual((await self.response(health))[0], 200)

    async def test_disconnect_kills_entire_process_group(self):
        reader, writer = await self.connect(model="descendant")
        child = await self.wait_file("child.pid")
        leader = await self.wait_file("leader.pid")
        writer.close()
        await writer.wait_closed()
        await self.wait_idle()
        with self.assertRaises(ProcessLookupError):
            os.kill(leader, 0)
        # A killed grandchild may briefly remain a zombie until host PID 1 reaps it.
        proc_stat = Path(f"/proc/{child}/stat")
        if proc_stat.exists():
            self.assertEqual(proc_stat.read_text().split()[2], "Z")

    async def test_cancelling_handler_kills_process(self):
        await self.connect(model="sleep")
        pid = await self.wait_file("leader.pid")
        pending = list(self.bridge.handlers)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        self.assertEqual(self.bridge.active, 0)

    async def test_real_server_socket_mode_and_shutdown_cleanup(self):
        self.server.close()
        await self.server.wait_closed()
        self.path.unlink(missing_ok=True)
        proc = await asyncio.create_subprocess_exec(
            "python3", "-I", str(BRIDGE_PATH), "--socket", str(self.path),
            "--work-dir", str(self.work), "--claude-bin", str(self.executable),
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            async with asyncio.timeout(3):
                while not self.path.exists():
                    if proc.returncode is not None:
                        self.fail("Server exited during startup")
                    await asyncio.sleep(0.01)
            self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
            self.assertTrue(await asyncio.to_thread(bridge.check_health, str(self.path)))
            proc.send_signal(signal.SIGTERM)
            await asyncio.wait_for(proc.communicate(), 3)
            self.assertEqual(proc.returncode, 0)
            self.assertFalse(self.path.exists())
        finally:
            if proc.returncode is None:
                proc.kill()
                await proc.communicate()


FAKE_CODEX = """#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
prompt = sys.stdin.read()
images = [args[i + 1] for i, a in enumerate(args) if a == '-i']
resumed = 'resume' in args
thread = args[args.index('resume') + 1] if resumed else 'thread-' + str(len(os.listdir('.')))
model = args[args.index('-m') + 1] if '-m' in args else 'default'
if model == 'flaky' and resumed:
    sys.exit(1)
if model == 'failure':
    sys.stderr.write('SECRET AUTH PAYLOAD')
    sys.exit(1)
if not resumed:
    print(json.dumps({'type': 'thread.started', 'thread_id': thread}))
reply = {'args': args, 'prompt': prompt, 'thread': thread, 'images': [[p, os.path.exists(p)] for p in images]}
text = json.dumps(reply)
if model == 'prose':
    text = 'Here is the action:\\n```json\\n' + text + '\\n```'
print(json.dumps({'type': 'item.completed', 'item': {'type': 'agent_message', 'text': text}}))
print(json.dumps({'type': 'turn.completed'}))
"""


class CodexParsingTests(unittest.TestCase):
    def test_codex_events_yield_thread_and_reply(self):
        output = "\n".join(json.dumps(e) for e in [
            {"type": "thread.started", "thread_id": "t1"},
            {"type": "item.completed", "item": {"type": "agent_message", "text": 'Done: {"type":"done","message":"ok"} thanks'}},
        ])
        self.assertEqual(bridge.parse_codex(output), ("t1", {"reply": {"type": "done", "message": "ok"}}))

    def test_codex_failures_are_rejected(self):
        for output in ["", json.dumps({"type": "turn.failed"}),
                       json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": "no json"}})]:
            with self.subTest(output=output), self.assertRaises(bridge.BridgeError) as error:
                bridge.parse_codex(output)
            self.assertEqual(error.exception.status, 502)

    def test_session_validation(self):
        self.assertIsNone(bridge.plan_session(json.dumps(PLAN).encode()))
        self.assertEqual(bridge.plan_session(json.dumps(dict(PLAN, session="abc-1")).encode()), "abc-1")
        for bad in ["", "a b", "x" * 129, 5]:
            with self.subTest(bad=bad), self.assertRaises(bridge.BridgeError):
                bridge.plan_session(json.dumps(dict(PLAN, session=bad)).encode())


class CodexIntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.executable = self.root / "fake-codex"
        self.executable.write_text(FAKE_CODEX)
        self.executable.chmod(0o700)
        self.work = self.root / "work"
        self.work.mkdir()
        self.path = self.root / "bridge.sock"
        self.bridge = bridge.CodexBridge(str(self.executable), self.work, timeout=3)
        self.server = await asyncio.start_unix_server(self.bridge.handle, str(self.path), limit=bridge.MAX_HEADERS)

    async def asyncTearDown(self):
        self.server.close()
        await self.server.wait_closed()
        await self.bridge.close()

    async def plan(self, session="s1", model="default", content=None, **fields):
        plan = dict(PLAN, model=model, session=session, content=content or PLAN["content"], **fields)
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        body = json.dumps(plan).encode()
        writer.write(f"POST /plan HTTP/1.1\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
        await writer.drain()
        raw = await asyncio.wait_for(reader.read(), 5)
        writer.close()
        await writer.wait_closed()
        header, payload = raw.split(b"\r\n\r\n", 1)
        return int(header.split()[1]), json.loads(payload)

    async def test_first_step_starts_a_read_only_thread_with_the_rules(self):
        image = {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
        status, result = await self.plan(content=[{"type": "text", "text": "screen"}, image])
        self.assertEqual(status, 200)
        reply = result["reply"]
        args = reply["args"]
        self.assertEqual(args[0], "exec")
        self.assertNotIn("resume", args)
        for value in ('sandbox_mode="read-only"', 'approval_policy="never"', "--json", "--skip-git-repo-check"):
            self.assertIn(value, args)
        self.assertNotIn("-m", args, "default keeps the CLI's own model")
        self.assertIn('model_reasoning_effort="low"', args)
        self.assertTrue(reply["prompt"].startswith(bridge.CODEX_ROLE))
        self.assertIn(PLAN["system"], reply["prompt"])
        self.assertTrue(reply["prompt"].endswith("screen"))
        disabled = [args[i + 1] for i, a in enumerate(args) if a == "--disable"]
        for feature in ("code_mode_host", "shell_tool", "multi_agent", "computer_use"):
            self.assertIn(feature, disabled)
        self.assertEqual(len(reply["images"]), 1)
        self.assertTrue(reply["images"][0][1], "the image existed while Codex ran")
        self.assertFalse(Path(reply["images"][0][0]).exists(), "request images are removed afterwards")
        self.assertEqual(list(self.work.iterdir()), [])

    async def test_next_steps_resume_the_same_thread_per_session(self):
        _, first = await self.plan()
        thread = first["reply"]["thread"]
        _, second = await self.plan()
        args = second["reply"]["args"]
        self.assertEqual(args[:3], ["exec", "resume", thread])
        self.assertTrue(second["reply"]["prompt"].startswith(bridge.CODEX_FOLLOW_UP))
        self.assertNotIn(PLAN["system"], second["reply"]["prompt"])
        _, other = await self.plan(session="s2")
        self.assertNotIn("resume", other["reply"]["args"])

    async def test_a_lost_thread_falls_back_to_a_new_one(self):
        _, first = await self.plan(model="flaky")
        status, second = await self.plan(model="flaky")
        self.assertEqual(status, 200)
        self.assertNotIn("resume", second["reply"]["args"])
        self.assertTrue(second["reply"]["prompt"].startswith(bridge.CODEX_ROLE))
        self.assertEqual(self.bridge.threads["s1"], second["reply"]["thread"])
        self.assertIn("-m", second["reply"]["args"])

    async def test_default_model_and_reasoning_effort_are_configurable(self):
        self.bridge.default_model = "gpt-6-luna"
        self.bridge.reasoning_effort = "minimal"
        _, result = await self.plan()
        args = result["reply"]["args"]
        self.assertEqual(args[args.index("-m") + 1], "gpt-6-luna")
        self.assertIn('model_reasoning_effort="minimal"', args)
        self.bridge.reasoning_effort = ""
        _, result = await self.plan(model="gpt-6-sol", session="s9")
        args = result["reply"]["args"]
        self.assertEqual(args[args.index("-m") + 1], "gpt-6-sol")
        self.assertFalse(any("model_reasoning_effort" in a for a in args))

    async def test_follow_up_continues_a_thread_and_full_content_starts_one(self):
        image = {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
        delta = [{"type": "text", "text": "delta"}, image]
        _, first = await self.plan(content=[{"type": "text", "text": "full"}], followUp=delta)
        self.assertTrue(first["reply"]["prompt"].endswith("full"), "a first turn always gets the full content")
        self.assertEqual(first["reply"]["images"], [])
        _, second = await self.plan(content=[{"type": "text", "text": "full"}], followUp=delta)
        self.assertEqual(second["reply"]["prompt"], f"{bridge.CODEX_FOLLOW_UP}\n\ndelta")
        self.assertTrue(second["reply"]["images"][0][1])

    async def test_threads_rotate_and_sessions_close(self):
        with mock.patch.object(bridge, "THREAD_MAX_TURNS", 2):
            replies = [(await self.plan())[1]["reply"] for _ in range(3)]
        self.assertEqual(replies[1]["args"][:3], ["exec", "resume", replies[0]["thread"]])
        self.assertNotIn("resume", replies[2]["args"], "a fresh thread after THREAD_MAX_TURNS turns")
        self.assertEqual(self.bridge.turn_counts["s1"], 1)
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        writer.write(b'POST /session/close HTTP/1.1\r\nContent-Length: 16\r\n\r\n{"session":"s1"}')
        raw = await asyncio.wait_for(reader.read(), 5)
        writer.close()
        self.assertTrue(raw.startswith(b"HTTP/1.1 200 ") and raw.endswith(b'{"ok": true}'), raw)
        self.assertEqual((self.bridge.threads, self.bridge.locks, self.bridge.turn_counts), ({}, {}, {}))
        _, after = await self.plan()
        self.assertNotIn("resume", after["reply"]["args"])

    async def test_fallback_needs_enough_deadline_budget(self):
        await self.plan(model="flaky")
        status, _ = await self.plan(model="flaky", deadlineMs=10000)
        self.assertEqual(status, 502, "no fresh-thread retry with under FALLBACK_MIN_BUDGET left")
        self.assertNotIn("s1", self.bridge.threads)
        await self.plan(model="flaky")
        with mock.patch.object(bridge, "FALLBACK_MIN_BUDGET", 1):
            status, result = await self.plan(model="flaky", deadlineMs=10000)
        self.assertEqual(status, 200)
        self.assertNotIn("resume", result["reply"]["args"])

    async def test_prose_replies_and_sanitized_failures(self):
        status, result = await self.plan(model="prose")
        self.assertEqual(status, 200)
        self.assertIn("args", result["reply"])
        status, result = await self.plan(model="failure", session="s3")
        self.assertEqual(status, 502)
        self.assertNotIn("SECRET", json.dumps(result))


FAKE_APP_SERVER = """#!/usr/bin/env python3
import json, os, sys
argv = sys.argv[1:]
threads = {}
turns = [0]
pending = {}  # thread -> turn id left running
if os.environ.get('FAKE_INIT') == 'hang' and not os.path.exists('../init-once'):
    open('../init-once', 'w').close()
    for line in sys.stdin:
        pass
    sys.exit(0)
def send(message):
    sys.stdout.write(json.dumps(message) + '\\n')
    sys.stdout.flush()
def log(name, value):
    with open(os.path.join('..', name), 'a') as f:
        f.write(value + '\\n')
for line in sys.stdin:
    message = json.loads(line)
    method, params, request_id = message.get('method'), message.get('params') or {}, message.get('id')
    if method == 'initialize':
        send({'id': request_id, 'result': {}})
    elif method == 'thread/start':
        thread = 'thread-%d' % (len(threads) + 1)
        threads[thread] = params
        send({'id': request_id, 'result': {'thread': {'id': thread}}})
    elif method in ('thread/unsubscribe', 'thread/archive'):
        log('unloaded', method + ' ' + params['threadId'])
        send({'id': request_id, 'result': {}})
    elif method == 'turn/interrupt':
        with open('interrupted', 'w') as f:
            f.write(params['turnId'])
        send({'id': request_id, 'result': {}})
        if pending.get(params['threadId']) == params['turnId']:
            del pending[params['threadId']]
            send({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                  'turn': {'id': params['turnId'], 'status': 'interrupted'}}})
    elif method == 'turn/start':
        thread = params['threadId']
        text = params['input'][0]['text']
        if 'CRASH' in text:
            sys.exit(1)
        turns[0] += 1
        turn = 'turn-%d' % turns[0]
        send({'id': request_id, 'result': {'turn': {'id': turn}}})
        if 'SLOW' in text:
            pending[thread] = turn
            continue
        if 'STUCK' in text:
            continue
        if 'STALE' in text:
            # Events of an earlier turn of this thread, arriving late.
            send({'method': 'item/completed', 'params': {'threadId': thread, 'turnId': 'turn-old',
                  'item': {'type': 'agentMessage', 'text': json.dumps({'stale': True})}}})
            send({'method': 'turn/completed', 'params': {'threadId': thread, 'turn': {'id': 'turn-old', 'status': 'completed'}}})
        images = [[i['path'], os.path.exists(i['path'])] for i in params['input'][1:]]
        reply = {'argv': argv, 'pid': os.getpid(), 'thread': thread, 'start': threads.get(thread),
                 'text': text, 'model': params.get('model'), 'effort': params.get('effort'), 'images': images}
        send({'method': 'item/started', 'params': {'threadId': thread, 'turnId': turn, 'item': {'type': 'reasoning'}}})
        send({'method': 'item/completed', 'params': {'threadId': thread, 'turnId': turn,
              'item': {'type': 'agentMessage', 'text': json.dumps(reply)}}})
        status = 'failed' if 'FAIL' in text else 'completed'
        send({'method': 'turn/completed', 'params': {'threadId': thread, 'turn': {'id': turn, 'status': status}}})
"""


class CodexAppServerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.executable = self.root / "fake-codex"
        self.executable.write_text(FAKE_APP_SERVER)
        self.executable.chmod(0o700)
        self.work = self.root / "work"
        self.work.mkdir()
        self.path = self.root / "bridge.sock"
        self.bridge = bridge.CodexAppBridge(str(self.executable), self.work, timeout=2, default_model="gpt-6-luna")
        self.server = await asyncio.start_unix_server(self.bridge.handle, str(self.path), limit=bridge.MAX_HEADERS)

    async def asyncTearDown(self):
        self.server.close()
        await self.server.wait_closed()
        await self.bridge.close()

    async def plan(self, text="screen", session="s1", model="default", image=False, **fields):
        content = [{"type": "text", "text": text}]
        if image:
            content.append({"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}})
        body = json.dumps(dict(PLAN, model=model, session=session, content=content, **fields)).encode()
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        writer.write(f"POST /plan HTTP/1.1\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
        await writer.drain()
        raw = await asyncio.wait_for(reader.read(), 6)
        writer.close()
        await writer.wait_closed()
        header, payload = raw.split(b"\r\n\r\n", 1)
        return int(header.split()[1]), json.loads(payload)

    async def test_first_step_starts_a_read_only_thread_with_the_rules_as_instructions(self):
        status, result = await self.plan(image=True)
        self.assertEqual(status, 200, result)
        reply = result["reply"]
        self.assertEqual(reply["argv"][0], "app-server")
        for value in ('sandbox_mode="read-only"', 'approval_policy="never"', "code_mode_host", "multi_agent"):
            self.assertIn(value, reply["argv"])
        start = reply["start"]
        self.assertEqual((start["sandbox"], start["approvalPolicy"], start["model"]), ("read-only", "never", "gpt-6-luna"))
        self.assertTrue(start["developerInstructions"].startswith(bridge.CODEX_ROLE))
        self.assertIn(PLAN["system"], start["developerInstructions"])
        self.assertEqual(reply["text"], "screen")
        self.assertEqual((reply["model"], reply["effort"]), ("gpt-6-luna", "low"))
        self.assertTrue(reply["images"][0][1], "the image existed during the turn")
        self.assertEqual(list(self.work.iterdir()), [], "request images are removed afterwards")

    async def test_later_steps_reuse_one_server_and_the_session_thread(self):
        _, first = await self.plan()
        _, second = await self.plan(model="gpt-6-sol")
        self.assertEqual(second["reply"]["pid"], first["reply"]["pid"], "one long-lived app-server")
        self.assertEqual(second["reply"]["thread"], first["reply"]["thread"])
        self.assertTrue(second["reply"]["text"].startswith(bridge.CODEX_FOLLOW_UP))
        self.assertEqual(second["reply"]["model"], "gpt-6-sol", "per-task model choice")
        _, other = await self.plan(session="s2")
        self.assertNotEqual(other["reply"]["thread"], first["reply"]["thread"])

    async def test_a_crashed_server_is_restarted_with_new_threads(self):
        _, first = await self.plan()
        status, _ = await self.plan(text="CRASH")
        self.assertEqual(status, 502)
        status, after = await self.plan()
        self.assertEqual(status, 200, after)
        self.assertNotEqual(after["reply"]["pid"], first["reply"]["pid"])
        self.assertIsNotNone(after["reply"]["start"], "a fresh thread on the new server")

    async def test_failed_turns_and_timeouts_are_reported(self):
        status, result = await self.plan(text="FAIL")
        self.assertEqual(status, 502)
        self.assertEqual(result, {"error": "Invalid Codex response"})
        status, _ = await self.plan(text="SLOW", session="s3")
        self.assertEqual(status, 504)
        self.assertEqual((self.work / "interrupted").read_text(), "turn-2")

    async def close_session(self, body):
        reader, writer = await asyncio.open_unix_connection(str(self.path))
        writer.write(f"POST /session/close HTTP/1.1\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
        raw = await asyncio.wait_for(reader.read(), 5)
        writer.close()
        await writer.wait_closed()
        header, payload = raw.split(b"\r\n\r\n", 1)
        return int(header.split()[1]), json.loads(payload)

    async def unloaded(self, thread):
        log = self.root / "unloaded"
        expected = {f"thread/unsubscribe {thread}", f"thread/archive {thread}"}
        async with asyncio.timeout(3):
            while not log.exists() or not expected <= set(log.read_text().split("\n")):
                await asyncio.sleep(0.01)

    async def test_a_failed_initialize_is_retried(self):
        with mock.patch.dict(os.environ, {"FAKE_INIT": "hang"}):
            status, _ = await self.plan()
            self.assertEqual(status, 504)
            self.assertFalse(self.bridge.server.running(), "a server that never initialized is not kept")
            status, result = await self.plan()
        self.assertEqual(status, 200, result)
        self.assertTrue(self.bridge.server.initialized)

    async def test_late_events_of_another_turn_are_ignored(self):
        await self.plan()
        status, result = await self.plan(text="STALE")
        self.assertEqual(status, 200, result)
        self.assertNotIn("stale", result["reply"])
        self.assertIn("STALE", result["reply"]["text"])

    async def test_an_interrupted_turn_is_awaited_before_the_thread_is_reused(self):
        _, first = await self.plan()
        status, _ = await self.plan(text="SLOW")
        self.assertEqual(status, 504)
        _, after = await self.plan()
        self.assertEqual(after["reply"]["thread"], first["reply"]["thread"], "the turn ended: the thread is safe")

    async def test_a_turn_that_does_not_end_abandons_its_thread(self):
        _, first = await self.plan()
        thread = first["reply"]["thread"]
        with mock.patch.object(bridge, "INTERRUPT_GRACE", 0.3):
            status, _ = await self.plan(text="STUCK")
        self.assertEqual(status, 504)
        self.assertNotIn(thread, self.bridge.thread_generation)
        await self.unloaded(thread)
        _, after = await self.plan()
        self.assertNotEqual(after["reply"]["thread"], thread)
        self.assertIsNotNone(after["reply"]["start"], "a fresh thread with the full rules")

    async def test_session_close_unloads_the_thread(self):
        _, first = await self.plan()
        thread = first["reply"]["thread"]
        self.assertEqual(await self.close_session(b'{"session":"s1"}'), (200, {"ok": True}))
        state = (self.bridge.threads, self.bridge.locks, self.bridge.turn_counts, self.bridge.thread_generation)
        self.assertEqual(state, ({}, {}, {}, {}))
        await self.unloaded(thread)
        self.assertEqual(await self.close_session(b'{"session":"unknown"}'), (200, {"ok": True}))
        self.assertEqual((await self.close_session(b'{"session":"a b"}'))[0], 400)
        _, after = await self.plan()
        self.assertNotEqual(after["reply"]["thread"], thread)

    async def test_evicted_and_sessionless_threads_are_unloaded(self):
        with mock.patch.object(bridge, "MAX_THREADS", 1):
            _, first = await self.plan()
            _, second = await self.plan(session="s2")
        self.assertEqual(list(self.bridge.threads), ["s2"])
        self.assertEqual(list(self.bridge.thread_generation), [second["reply"]["thread"]])
        await self.unloaded(first["reply"]["thread"])
        _, sessionless = await self.plan(session=None)
        await self.unloaded(sessionless["reply"]["thread"])
        self.assertEqual(list(self.bridge.thread_generation), [second["reply"]["thread"]])

    async def test_follow_up_rotation_and_deadline(self):
        delta = [{"type": "text", "text": "delta"}]
        with mock.patch.object(bridge, "THREAD_MAX_TURNS", 2):
            _, first = await self.plan(text="full", followUp=delta)
            _, second = await self.plan(text="full", followUp=delta)
            _, third = await self.plan(text="full", followUp=delta)
        self.assertEqual(first["reply"]["text"], "full")
        self.assertEqual(second["reply"]["text"], f"{bridge.CODEX_FOLLOW_UP}\n\ndelta")
        self.assertEqual(second["reply"]["thread"], first["reply"]["thread"])
        self.assertNotEqual(third["reply"]["thread"], first["reply"]["thread"])
        self.assertEqual(third["reply"]["text"], "full")
        self.bridge.timeout = 30
        started = asyncio.get_running_loop().time()
        status, _ = await self.plan(text="SLOW", session="s3", deadlineMs=3000)
        self.assertEqual(status, 504)
        self.assertLess(asyncio.get_running_loop().time() - started, 1 + bridge.INTERRUPT_GRACE + 1)

    async def test_the_server_is_recycled_when_worn_or_idle(self):
        self.bridge.server.max_threads = 1
        _, first = await self.plan()
        self.bridge.server.idle = 0.2
        _, second = await self.plan(session="s2")
        self.assertNotEqual(second["reply"]["pid"], first["reply"]["pid"], "recycled after max_threads")
        self.bridge.server.max_threads = 100
        async with asyncio.timeout(3):
            while self.bridge.server.proc is not None:
                await asyncio.sleep(0.01)
        _, third = await self.plan(session="s2")
        self.assertNotEqual(third["reply"]["pid"], second["reply"]["pid"])
        self.assertIsNotNone(third["reply"]["start"], "threads of a stopped server are not resumed")


if __name__ == "__main__":
    unittest.main()
