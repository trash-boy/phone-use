#!/usr/bin/env python3
"""Local, bounded HTTP-to-planner bridge over an explicitly selected Unix socket.

The planner is the host Claude Code CLI by default, or the host Codex agent with
--codex-bin. Codex keeps one agent thread per gateway session, so the agent's
own conversation memory carries across the steps and tasks of a connection."""

import argparse
import asyncio
import base64
import binascii
import errno
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import stat
import sys
import tempfile

MAX_REQUEST = 16 * 1024 * 1024
MAX_OUTPUT = 1024 * 1024
MAX_HEADERS = 16 * 1024
TIMEOUT = 90
# Gateway session ids name Codex threads; at most this many are remembered.
SESSION_PATTERN = r"[A-Za-z0-9._:-]{1,128}"
MAX_THREADS = 64
# Codex is an agent with its own tools. The phone tools named in the rules are run by the
# gateway, not by Codex: with a small model it searched its own tools for them, found none and
# replied that it could not control the phone. Say so, and disable the tool-providing features.
CODEX_ROLE = ("ROLE: You are the planning brain of an external phone controller. You have NO tools: never call "
              "any tool, function, shell, exec, search or sub-agent. Read the attached phone screenshot and the "
              "text, then reply with exactly one JSON object describing the next step. The phone tools named in "
              "the rules below (iphone_*) are executed by the controller, not by you, so they are always "
              "available.")
CODEX_FOLLOW_UP = ("Next step of the same phone task. You still have no tools; the controller executes the "
                   "iphone_* action you choose. Follow the rules from the first message of this conversation "
                   "and reply with exactly one JSON object and nothing else.")
CODEX_DISABLED_FEATURES = (
    "code_mode_host", "shell_tool", "unified_exec", "browser_use", "browser_use_external", "computer_use",
    "apps", "multi_agent", "plugins", "skill_search", "image_generation", "sleep_tool", "tool_suggest",
    "goals", "hooks", "view_image",
)
IMAGE_SUFFIX = {"image/png": "png", "image/jpeg": "jpg", "image/gif": "gif", "image/webp": "webp"}
# A pre-started CLI waits on stdin for the next step of the same task. Unused
# ones are reclaimed after this long (a task step normally follows in seconds).
WARM_IDLE = 300


class BridgeError(Exception):
    def __init__(self, status, message):
        self.status = status
        self.message = message
        super().__init__(message)


def translate_plan(body):
    try:
        plan = json.loads(body)
    except (ValueError, UnicodeError):
        raise BridgeError(400, "Invalid JSON request") from None
    if not isinstance(plan, dict):
        raise BridgeError(400, "Expected a plan object")
    model, system, content = (plan.get(k) for k in ("model", "system", "content"))
    if not isinstance(model, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}", model):
        raise BridgeError(400, "Invalid model")
    if not isinstance(system, str) or "\x00" in system:
        raise BridgeError(400, "Invalid system prompt")
    if not isinstance(content, list) or not content:
        raise BridgeError(400, "Expected nonempty content")
    blocks = []
    for block in content:
        if not isinstance(block, dict):
            raise BridgeError(400, "Invalid content block")
        if block.get("type") == "text" and isinstance(block.get("text"), str):
            blocks.append({"type": "text", "text": block["text"]})
        elif block.get("type") == "image_url":
            image = block.get("image_url")
            url = image.get("url") if isinstance(image, dict) else None
            match = re.fullmatch(r"data:(image/(?:png|jpeg|gif|webp));base64,([A-Za-z0-9+/]*={0,2})", url) if isinstance(url, str) else None
            if not match:
                raise BridgeError(400, "Expected a supported base64 image data URL")
            try:
                decoded = base64.b64decode(match[2], validate=True)
            except (ValueError, binascii.Error):
                raise BridgeError(400, "Invalid base64 image") from None
            if not decoded:
                raise BridgeError(400, "Empty image")
            blocks.append({"type": "image", "source": {
                "type": "base64", "media_type": match[1], "data": match[2]}})
        else:
            raise BridgeError(400, "Unsupported content block")
    return model, system, blocks


def parse_result(output):
    try:
        events = [json.loads(line) for line in output.splitlines() if line.strip()]
        envelope = next(event for event in reversed(events) if isinstance(event, dict) and event.get("type") == "result")
        if envelope.get("is_error") or envelope.get("subtype", "success") != "success":
            raise ValueError
        text = envelope["result"]
        if not isinstance(text, str):
            raise ValueError
        text = text.strip()
        fenced = re.fullmatch(r"```(?:json)?\s*\n(.*?)\n```", text, re.DOTALL)
        if fenced:
            text = fenced[1]
        reply = json.loads(text)
        if not isinstance(reply, dict):
            raise ValueError
        return {"reply": reply}
    except (ValueError, KeyError, TypeError, UnicodeError, StopIteration):
        raise BridgeError(502, "Invalid Claude response") from None


def plan_session(body):
    """The optional gateway session id of a plan request (validated)."""
    try:
        plan = json.loads(body)
    except (ValueError, UnicodeError):
        return None
    session = plan.get("session") if isinstance(plan, dict) else None
    if session is None:
        return None
    if not isinstance(session, str) or not re.fullmatch(SESSION_PATTERN, session):
        raise BridgeError(400, "Invalid session")
    return session


def json_reply(text):
    """The first JSON object of an agent message, tolerating fences and surrounding prose."""
    text = text.strip()
    fenced = re.fullmatch(r"```(?:json)?\s*\n(.*?)\n```", text, re.DOTALL)
    if fenced:
        text = fenced[1]
    try:
        reply = json.loads(text)
    except ValueError:
        start = text.find("{")
        if start < 0:
            raise ValueError from None
        reply, _ = json.JSONDecoder().raw_decode(text[start:])
    if not isinstance(reply, dict):
        raise ValueError
    return reply


def parse_codex(output):
    """Thread id and reply from `codex exec --json` events."""
    thread, text = None, None
    try:
        for line in output.splitlines():
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if not isinstance(event, dict):
                continue
            kind = event.get("type")
            if kind == "thread.started" and isinstance(event.get("thread_id"), str):
                thread = event["thread_id"]
            elif kind == "item.completed":
                item = event.get("item")
                if isinstance(item, dict) and item.get("type") == "agent_message" and isinstance(item.get("text"), str):
                    text = item["text"]
            elif kind in ("turn.failed", "error"):
                raise ValueError
        if text is None:
            raise ValueError
        return thread, {"reply": json_reply(text)}
    except (ValueError, UnicodeError):
        raise BridgeError(502, "Invalid Codex response") from None


async def read_request(reader):
    try:
        header = await reader.readuntil(b"\r\n\r\n")
    except asyncio.LimitOverrunError:
        raise BridgeError(431, "Request headers too large") from None
    except asyncio.IncompleteReadError:
        raise BridgeError(400, "Incomplete request") from None
    if len(header) > MAX_HEADERS:
        raise BridgeError(431, "Request headers too large")
    try:
        lines = header[:-4].decode("ascii").split("\r\n")
        method, target, version = lines[0].split(" ")
        if version not in ("HTTP/1.0", "HTTP/1.1"):
            raise ValueError
        headers = {}
        for line in lines[1:]:
            key, value = line.split(":", 1)
            if not re.fullmatch(r"[!#$%&'*+.^_`|~0-9A-Za-z-]+", key):
                raise ValueError
            key = key.lower()
            if key in headers:
                raise ValueError
            headers[key] = value.strip()
        if "transfer-encoding" in headers or "expect" in headers:
            raise ValueError
        length = headers.get("content-length", "0")
        if not re.fullmatch(r"[0-9]{1,10}", length):
            raise ValueError
        length = int(length)
    except ValueError:
        raise BridgeError(400, "Invalid HTTP request") from None
    if length > MAX_REQUEST:
        raise BridgeError(413, "Request body too large")
    if method == "POST" and "content-length" not in headers:
        raise BridgeError(411, "Content-Length required")
    try:
        body = await reader.readexactly(length)
    except asyncio.IncompleteReadError:
        raise BridgeError(400, "Incomplete request body") from None
    return method, target, body


class Bridge:
    def __init__(self, claude_bin, work_dir, timeout=TIMEOUT, warm_idle=WARM_IDLE):
        self.claude_bin = claude_bin
        self.work_dir = str(work_dir)
        self.timeout = timeout
        self.warm_idle = warm_idle
        self.active = 0
        self.handlers = set()
        # (key, process, expiry task) of at most one pre-started CLI.
        self.warm = None
        self.background = set()
        self.closed = False

    async def spawn(self, model, system):
        env = os.environ.copy()
        env.pop("CLAUDECODE", None)
        env.pop("CLAUDE_CODE_ENTRYPOINT", None)
        command = [self.claude_bin, "-p", "--input-format", "stream-json",
                   "--output-format", "stream-json", "--verbose", "--tools", "", "--strict-mcp-config",
                   "--mcp-config", '{"mcpServers":{}}', "--setting-sources", "",
                   "--disable-slash-commands", "--no-session-persistence", "--safe-mode",
                   "--permission-mode", "dontAsk", "--system-prompt", system,
                   "--model", model, "--settings", '{"disableAllHooks":true}']
        try:
            return await asyncio.create_subprocess_exec(
                *command, cwd=self.work_dir, env=env, start_new_session=True,
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE)
        except (OSError, ValueError):
            raise BridgeError(502, "Could not start Claude") from None

    @staticmethod
    async def kill(proc):
        # Kill descendants even when the CLI leader has already exited.
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        await proc.wait()

    def take_warm(self, key):
        """Return the pre-started CLI for key, or None. A mismatched one is released."""
        warm, self.warm = self.warm, None
        if warm is None:
            return None
        warm_key, proc, expiry = warm
        expiry.cancel()
        if warm_key == key and proc.returncode is None:
            return proc
        self.run_background(self.kill(proc))
        return None

    def run_background(self, coroutine):
        task = asyncio.create_task(coroutine)
        self.background.add(task)
        task.add_done_callback(self.background.discard)
        return task

    async def prewarm(self, key):
        """Start the CLI for the next step now, so its startup overlaps device work."""
        if self.closed:
            return
        try:
            proc = await self.spawn(*key)
        except BridgeError:
            return
        if self.closed:
            await self.kill(proc)
            return
        self.take_warm(None)  # Keep at most one: release any other pre-started CLI.

        async def expire():
            await asyncio.sleep(self.warm_idle)
            if self.warm is not None and self.warm[1] is proc:
                self.warm = None
                await self.kill(proc)

        self.warm = (key, proc, self.run_background(expire()))

    async def close(self):
        self.closed = True
        warm, self.warm = self.warm, None
        if warm is not None:
            warm[2].cancel()
            await self.kill(warm[1])
        pending = list(self.background)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)

    async def plan(self, body, reader):
        model, system, blocks = translate_plan(body)
        if self.active >= 2:
            raise BridgeError(503, "Bridge is busy")
        self.active += 1
        proc = None
        tasks = []
        succeeded = False
        try:
            proc = self.take_warm((model, system)) or await self.spawn(model, system)
            used = 0

            async def drain(stream, retain):
                nonlocal used
                chunks = []
                while True:
                    chunk = await stream.read(65536)
                    if not chunk:
                        break
                    used += len(chunk)
                    if used > MAX_OUTPUT:
                        raise BridgeError(502, "Claude output exceeded limit")
                    if retain:
                        chunks.append(chunk)
                return b"".join(chunks)

            async def exchange():
                outgoing = {"type": "user", "message": {"role": "user", "content": blocks}}
                proc.stdin.write((json.dumps(outgoing) + "\n").encode())
                try:
                    await proc.stdin.drain()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                finally:
                    proc.stdin.close()
                stdout, _ = await asyncio.gather(drain(proc.stdout, True), drain(proc.stderr, False))
                await proc.wait()
                if proc.returncode != 0:
                    raise BridgeError(502, "Claude execution failed")
                return parse_result(stdout)

            # Additional data is also rejected: each connection carries one request.
            work = asyncio.create_task(exchange())
            disconnected = asyncio.create_task(reader.read(1))
            tasks = [work, disconnected]
            done, _ = await asyncio.wait(tasks, timeout=self.timeout, return_when=asyncio.FIRST_COMPLETED)
            if disconnected in done:
                raise BridgeError(499, "Client disconnected")
            if work not in done:
                raise BridgeError(504, "Claude request timed out")
            result = work.result()
            succeeded = True
            return result
        finally:
            if proc is not None:
                await self.kill(proc)
            for task in tasks:
                task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)
            self.active -= 1
            # A task plans step after step with the same model and system prompt.
            if succeeded and not self.closed:
                self.run_background(self.prewarm((model, system)))

    async def handle(self, reader, writer):
        task = asyncio.current_task()
        self.handlers.add(task)
        try:
            try:
                method, target, body = await asyncio.wait_for(read_request(reader), 15)
                if method == "GET" and target == "/health":
                    status, response = 200, {"ok": True}
                elif method == "POST" and target == "/plan":
                    response = await self.plan(body, reader)
                    status = 200
                else:
                    raise BridgeError(404, "Not found")
            except BridgeError as exc:
                status, response = exc.status, {"error": exc.message}
            except asyncio.TimeoutError:
                status, response = 408, {"error": "Request timed out"}
            except (ConnectionError, OSError):
                return
            except Exception:
                status, response = 500, {"error": "Internal bridge error"}
            payload = json.dumps(response, ensure_ascii=True).encode()
            writer.write((f"HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\n"
                          f"Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n").encode() + payload)
            await writer.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionError, OSError):
                pass
            self.handlers.discard(task)


class CodexBridge(Bridge):
    """Plans each step with the host Codex agent, resuming one thread per gateway session."""

    def __init__(self, codex_bin, work_dir, timeout=TIMEOUT, default_model=None, reasoning_effort="low"):
        super().__init__(codex_bin, work_dir, timeout)
        self.threads = {}  # session -> Codex thread id, oldest first
        self.locks = {}
        # Phone steps need quick replies: a small model with low reasoning planned a step in about 3 s,
        # the CLI's own gpt-6-sol with medium reasoning took 8-25 s.
        self.default_model = default_model
        self.reasoning_effort = reasoning_effort

    def command(self, model, thread, images):
        options = ["--skip-git-repo-check", "--json",
                   "-c", 'sandbox_mode="read-only"', "-c", 'approval_policy="never"']
        if self.reasoning_effort:
            options += ["-c", f'model_reasoning_effort="{self.reasoning_effort}"']
        for feature in CODEX_DISABLED_FEATURES:
            options += ["--disable", feature]
        model = self.resolve_model(model)
        if model:
            options += ["-m", model]
        for image in images:
            options += ["-i", image]
        # No prompt argument: Codex reads the prompt from stdin.
        if thread:
            return [self.claude_bin, "exec", "resume", thread, *options]
        return [self.claude_bin, "exec", *options]

    async def run(self, command, prompt, reader):
        try:
            proc = await asyncio.create_subprocess_exec(
                *command, cwd=self.work_dir, start_new_session=True,
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE)
        except (OSError, ValueError):
            raise BridgeError(502, "Could not start Codex") from None
        tasks = []
        try:
            used = 0

            async def drain(stream, retain):
                nonlocal used
                chunks = []
                while True:
                    chunk = await stream.read(65536)
                    if not chunk:
                        break
                    used += len(chunk)
                    if used > MAX_OUTPUT:
                        raise BridgeError(502, "Codex output exceeded limit")
                    if retain:
                        chunks.append(chunk)
                return b"".join(chunks)

            async def exchange():
                proc.stdin.write(prompt.encode())
                try:
                    await proc.stdin.drain()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                finally:
                    proc.stdin.close()
                stdout, _ = await asyncio.gather(drain(proc.stdout, True), drain(proc.stderr, False))
                await proc.wait()
                if proc.returncode != 0:
                    raise BridgeError(502, "Codex execution failed")
                return parse_codex(stdout.decode("utf-8", "replace"))

            work = asyncio.create_task(exchange())
            disconnected = asyncio.create_task(reader.read(1))
            tasks = [work, disconnected]
            done, _ = await asyncio.wait(tasks, timeout=self.timeout, return_when=asyncio.FIRST_COMPLETED)
            if disconnected in done:
                raise BridgeError(499, "Client disconnected")
            if work not in done:
                raise BridgeError(504, "Codex request timed out")
            return work.result()
        finally:
            await self.kill(proc)
            for task in tasks:
                task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)

    def current_thread(self, session):
        return self.threads.get(session) if session else None

    async def first_turn(self, model, system, text, images, reader):
        """Start a thread with the full rules; returns (thread id, result)."""
        return await self.run(self.command(model, None, images), f"{CODEX_ROLE}\n\n{system}\n\n{text}", reader)

    async def next_turn(self, model, thread, text, images, reader):
        _, result = await self.run(self.command(model, thread, images), f"{CODEX_FOLLOW_UP}\n\n{text}", reader)
        return result

    def resolve_model(self, model):
        """The explicit model to request, or None to keep the CLI's own."""
        model = self.default_model if model == "default" and self.default_model else model
        return None if model == "default" else model

    def remember(self, session, thread):
        self.threads.pop(session, None)
        self.threads[session] = thread
        while len(self.threads) > MAX_THREADS:
            oldest = next(iter(self.threads))
            del self.threads[oldest]
            self.locks.pop(oldest, None)

    async def plan(self, body, reader):
        model, system, blocks = translate_plan(body)
        session = plan_session(body)
        if self.active >= 4:
            raise BridgeError(503, "Bridge is busy")
        self.active += 1
        images_dir = tempfile.mkdtemp(prefix=".request-", dir=self.work_dir)
        try:
            texts, images = [], []
            for index, block in enumerate(blocks):
                if block["type"] == "text":
                    texts.append(block["text"])
                    continue
                source = block["source"]
                path = os.path.join(images_dir, f"{index}.{IMAGE_SUFFIX[source['media_type']]}")
                with open(path, "wb") as image:
                    image.write(base64.b64decode(source["data"]))
                images.append(path)
            text = "\n\n".join(texts)
            lock = self.locks.setdefault(session, asyncio.Lock()) if session else asyncio.Lock()
            async with lock:
                thread = self.current_thread(session)
                if thread:
                    try:
                        result = await self.next_turn(model, thread, text, images, reader)
                        self.remember(session, thread)
                        return result
                    except BridgeError as exc:
                        if exc.status != 502:
                            raise
                        # The thread may be gone: start a fresh one with the full rules.
                        self.threads.pop(session, None)
                started, result = await self.first_turn(model, system, text, images, reader)
                if session and started:
                    self.remember(session, started)
                return result
        finally:
            shutil.rmtree(images_dir, ignore_errors=True)
            self.active -= 1


class AppServer:
    """One long-lived `codex app-server` over stdio JSON-RPC, so steps skip CLI startup."""

    def __init__(self, command, cwd):
        self.command = command
        self.cwd = cwd
        self.proc = None
        self.reader = None
        self.pending = {}
        self.turns = {}  # thread id -> queue of notifications for the running turn
        self.next_id = 0
        self.generation = 0
        self.lock = asyncio.Lock()

    def running(self):
        return self.proc is not None and self.proc.returncode is None and self.reader is not None and not self.reader.done()

    async def ensure(self):
        async with self.lock:
            if self.running():
                return
            await self.stop()
            try:
                self.proc = await asyncio.create_subprocess_exec(
                    *self.command, cwd=self.cwd, start_new_session=True, limit=MAX_OUTPUT,
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.DEVNULL)
            except (OSError, ValueError):
                raise BridgeError(502, "Could not start Codex") from None
            self.generation += 1
            self.reader = asyncio.create_task(self.read())
            await self.call("initialize", {"clientInfo": {"name": "phone-use-bridge", "version": "1"}})
            await self.send({"jsonrpc": "2.0", "method": "initialized"})

    async def send(self, message):
        try:
            self.proc.stdin.write((json.dumps(message) + "\n").encode())
            await self.proc.stdin.drain()
        except (BrokenPipeError, ConnectionResetError, AttributeError):
            raise BridgeError(502, "Codex app-server exited") from None

    async def call(self, method, params, timeout=30):
        self.next_id += 1
        request_id = self.next_id
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        try:
            await self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
            message = await asyncio.wait_for(future, timeout)
        except asyncio.TimeoutError:
            raise BridgeError(504, "Codex request timed out") from None
        finally:
            self.pending.pop(request_id, None)
        if "error" in message or not isinstance(message.get("result"), dict):
            raise BridgeError(502, "Codex request failed")
        return message["result"]

    async def read(self):
        try:
            while True:
                try:
                    line = await self.proc.stdout.readline()
                except (ValueError, asyncio.LimitOverrunError):
                    break  # An oversized message: drop this server rather than misparse the stream.
                if not line:
                    break
                try:
                    message = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(message, dict):
                    continue
                if "id" in message and "method" in message:
                    # Approvals and other server requests: tools are disabled, so refuse.
                    await self.send({"jsonrpc": "2.0", "id": message["id"],
                                     "error": {"code": -32601, "message": "Not supported by the phone-use bridge"}})
                elif "id" in message:
                    future = self.pending.get(message["id"])
                    if future is not None and not future.done():
                        future.set_result(message)
                else:
                    params = message.get("params")
                    queue = self.turns.get(params.get("threadId")) if isinstance(params, dict) else None
                    if queue is not None:
                        queue.put_nowait(message)
        except (BridgeError, ConnectionError, OSError):
            pass
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(BridgeError(502, "Codex app-server exited"))
            for queue in self.turns.values():
                queue.put_nowait(None)

    async def stop(self):
        proc, reader = self.proc, self.reader
        self.proc = self.reader = None
        if proc is not None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await proc.wait()
        if reader is not None:
            reader.cancel()
            await asyncio.gather(reader, return_exceptions=True)


class CodexAppBridge(CodexBridge):
    """Codex engine over one long-lived app-server: about 1.9 s per step instead of 3 s or more."""

    def __init__(self, codex_bin, work_dir, timeout=TIMEOUT, default_model=None, reasoning_effort="low"):
        super().__init__(codex_bin, work_dir, timeout, default_model, reasoning_effort)
        options = ["-c", 'sandbox_mode="read-only"', "-c", 'approval_policy="never"']
        for feature in CODEX_DISABLED_FEATURES:
            options += ["--disable", feature]
        self.server = AppServer([codex_bin, "app-server", *options], self.work_dir)
        self.thread_generation = {}

    def current_thread(self, session):
        thread = super().current_thread(session)
        # Threads of a replaced server are unknown to the new one.
        if thread and self.thread_generation.get(thread) != self.server.generation:
            self.threads.pop(session, None)
            return None
        return thread

    async def first_turn(self, model, system, text, images, reader):
        await self.server.ensure()
        params = {"cwd": self.work_dir, "sandbox": "read-only", "approvalPolicy": "never",
                  "developerInstructions": f"{CODEX_ROLE}\n\n{system}"}
        resolved = self.resolve_model(model)
        if resolved:
            params["model"] = resolved
        started = await self.server.call("thread/start", params)
        thread = started.get("thread", {}).get("id") if isinstance(started.get("thread"), dict) else None
        if not isinstance(thread, str):
            raise BridgeError(502, "Invalid Codex response")
        self.thread_generation[thread] = self.server.generation
        return thread, await self.turn(model, thread, text, images, reader)

    async def next_turn(self, model, thread, text, images, reader):
        await self.server.ensure()
        return await self.turn(model, thread, f"{CODEX_FOLLOW_UP}\n\n{text}", images, reader)

    async def turn(self, model, thread, text, images, reader):
        queue = asyncio.Queue()
        self.server.turns[thread] = queue
        tasks = []
        turn_id = None
        try:
            params = {"threadId": thread,
                      "input": [{"type": "text", "text": text}] + [{"type": "localImage", "path": p} for p in images]}
            resolved = self.resolve_model(model)
            if resolved:
                params["model"] = resolved
            if self.reasoning_effort:
                params["effort"] = self.reasoning_effort
            started = await self.server.call("turn/start", params)
            turn_id = started.get("turn", {}).get("id") if isinstance(started.get("turn"), dict) else None

            async def completion():
                message_text = None
                while True:
                    message = await queue.get()
                    if message is None:
                        raise BridgeError(502, "Codex app-server exited")
                    method, payload = message.get("method"), message.get("params") or {}
                    if method == "item/completed":
                        item = payload.get("item") if isinstance(payload.get("item"), dict) else {}
                        if item.get("type") == "agentMessage" and isinstance(item.get("text"), str):
                            message_text = item["text"]
                    elif method == "turn/completed":
                        status = payload.get("turn", {}).get("status") if isinstance(payload.get("turn"), dict) else None
                        if status != "completed" or message_text is None:
                            raise BridgeError(502, "Invalid Codex response")
                        try:
                            return {"reply": json_reply(message_text)}
                        except ValueError:
                            raise BridgeError(502, "Invalid Codex response") from None

            work = asyncio.create_task(completion())
            disconnected = asyncio.create_task(reader.read(1))
            tasks = [work, disconnected]
            done, _ = await asyncio.wait(tasks, timeout=self.timeout, return_when=asyncio.FIRST_COMPLETED)
            if work in done:
                return work.result()
            if turn_id:
                try:
                    await self.server.call("turn/interrupt", {"threadId": thread, "turnId": turn_id}, timeout=5)
                except BridgeError:
                    pass
            if disconnected in done:
                raise BridgeError(499, "Client disconnected")
            raise BridgeError(504, "Codex request timed out")
        finally:
            if self.server.turns.get(thread) is queue:
                del self.server.turns[thread]
            for task in tasks:
                task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)

    async def close(self):
        await super().close()
        await self.server.stop()


def prepare_socket(path):
    path = Path(path)
    if not path.is_absolute() or not path.parent.is_dir():
        raise ValueError("Socket requires an absolute path in an existing runtime directory")
    try:
        original = path.lstat()
    except FileNotFoundError:
        return
    if not stat.S_ISSOCK(original.st_mode):
        raise ValueError("Refusing an existing non-socket path")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
        probe.settimeout(1)
        try:
            probe.connect(str(path))
        except OSError as exc:
            if exc.errno != errno.ECONNREFUSED:
                raise ValueError("Cannot verify that the existing socket is stale") from None
        else:
            raise ValueError("Socket is already in use")
    current = path.lstat()
    if (current.st_dev, current.st_ino) != (original.st_dev, original.st_ino):
        raise ValueError("Socket path changed during startup")
    path.unlink()


async def serve(args):
    work_dir = Path(args.work_dir)
    if not work_dir.is_absolute():
        raise ValueError("Work directory must be absolute")
    work_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    if work_dir.is_symlink() or not work_dir.is_dir() or any(work_dir.iterdir()):
        raise ValueError("Work directory must be a dedicated empty directory")
    prepare_socket(args.socket)
    if args.codex_bin:
        engine = CodexAppBridge if args.codex_mode == "app-server" else CodexBridge
        bridge = engine(args.codex_bin, work_dir, default_model=args.codex_model,
                        reasoning_effort=args.reasoning_effort)
    else:
        bridge = Bridge(args.claude_bin, work_dir)
    # Bind synchronously so no client can connect before permissions are restricted.
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    identity = None
    server = None
    try:
        old_umask = os.umask(0o177)
        try:
            listener.bind(args.socket)
        finally:
            os.umask(old_umask)
        identity = os.lstat(args.socket)
        os.chmod(args.socket, 0o600)
        listener.listen(16)
        listener.setblocking(False)
        server = await asyncio.start_unix_server(bridge.handle, sock=listener, limit=MAX_HEADERS)
        stop = asyncio.Event()
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, stop.set)
        await stop.wait()
    finally:
        if server is not None:
            server.close()
            await server.wait_closed()
        listener.close()
        pending = list(bridge.handlers)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        await bridge.close()
        if identity is not None:
            try:
                current = os.lstat(args.socket)
                if (current.st_dev, current.st_ino) == (identity.st_dev, identity.st_ino):
                    os.unlink(args.socket)
            except FileNotFoundError:
                pass


def check_health(path):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(3)
        client.connect(path)
        client.sendall(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        data = bytearray()
        while True:
            chunk = client.recv(4096)
            if not chunk:
                break
            data.extend(chunk)
            if len(data) > MAX_HEADERS:
                return False
    header, body = bytes(data).split(b"\r\n\r\n", 1)
    return header.startswith(b"HTTP/1.1 200 ") and json.loads(body) == {"ok": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True, help="Absolute Unix socket in an existing runtime directory")
    parser.add_argument("--claude-bin", default="claude")
    parser.add_argument("--codex-bin", help="Plan with this Codex CLI instead of Claude Code")
    parser.add_argument("--codex-model", help="Codex model used when the gateway asks for `default`")
    parser.add_argument("--codex-mode", choices=("app-server", "exec"), default="app-server",
                        help="Keep one Codex app-server (default) or start `codex exec` for every step")
    parser.add_argument("--reasoning-effort", default="low",
                        help="Codex model_reasoning_effort for planning steps, or empty to keep the CLI's own")
    parser.add_argument("--work-dir", help="Dedicated empty absolute working directory (required unless --check)")
    parser.add_argument("--check", action="store_true", help="Check an already running bridge and exit")
    args = parser.parse_args()
    for value in (args.codex_model, args.reasoning_effort):
        if value and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,99}", value):
            parser.error("Invalid --codex-model or --reasoning-effort")
    if not args.check and not args.work_dir:
        parser.error("--work-dir is required unless --check is used")
    try:
        if args.check:
            if not check_health(args.socket):
                raise ValueError
            print('{"ok":true}')
        else:
            asyncio.run(serve(args))
    except (OSError, ValueError, KeyboardInterrupt):
        print("Bridge operation failed; check socket, executable and clean work directory", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
