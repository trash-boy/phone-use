#!/usr/bin/env python3
"""Restricted SSH entry point: deploy a release image without replacing VPS config.

Read one JSON request from stdin. Only this repository's immutable GHCR image is
accepted. Keep registry credentials temporary; preserve Compose/environment/data,
back up the previous release, and restore it when restart or health checks fail.
Install this file root-owned outside the writable stack; never run a copy supplied
in a deploy request. The caller must hold the stack deployment lock (see main).

The public site checked after a restart defaults to PUBLIC_URL. Override it with
`--public-url URL` in the forced SSH command (sudo resets the environment), or
with PHONE_USE_PUBLIC_URL when the environment is preserved. The API health
checks use that URL's origin. The deploy request itself can never change it.
"""

import argparse
import datetime
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


STACK = Path("/opt/stacks/iphone-control")
PUBLIC_URL = "https://amazingly.top/iphone/"
PUBLIC_URL_ENV = "PHONE_USE_PUBLIC_URL"
IMAGE_PATTERN = r"ghcr\.io/trash-boy/phone-use@sha256:[0-9a-f]{64}"


def validate(payload):
    rules = {
        "image": IMAGE_PATTERN,
        "revision": r"[0-9a-f]{40}",
        "branch": r"release-[A-Za-z0-9_.-]+",
        "runId": r"[1-9][0-9]{0,19}",
        "attempt": r"[1-9][0-9]{0,3}",
        "registryUser": r"[A-Za-z0-9][A-Za-z0-9_\[\]-]{0,99}",
        # Registry credentials are opaque and travel only on stdin, never in shell commands.
        "registryToken": r"[\x21-\x7e]{1,8192}",
    }
    if not isinstance(payload, dict):
        raise ValueError("Expected an object")
    for key, pattern in rules.items():
        if not isinstance(payload.get(key), str) or not re.fullmatch(pattern, payload[key]):
            raise ValueError(f"Invalid {key}")


def replace_image(source, image):
    if len(re.findall(r"(?m)^  control:\s*$", source)) != 1:
        raise ValueError("Expected one control service")
    pattern = r"(?m)^(    image:[ \t]*)(['\"]?)([A-Za-z0-9_./:@+-]+)\2([ \t]*(?:#.*)?)$"
    matches = list(re.finditer(pattern, source))
    if len(matches) != 1 or len(re.findall(r"(?m)^\s+image:", source)) != 1:
        raise ValueError("Expected exactly one image reference")
    match = matches[0]
    updated = source[:match.start(3)] + image + source[match.end(3):]
    return updated, match[3]


def atomic_write(path, content):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
            stream.close()
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def command(args, **kwargs):
    # Capture output: Compose's expanded environment and registry credentials must
    # never be echoed to Actions. Arguments never contain credentials.
    return subprocess.run(args, check=True, text=True, capture_output=True,
                          timeout=600, **kwargs).stdout


def http_check(url, expected, *, method="GET", require_json=False):
    request = urllib.request.Request(url, method=method)
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            status = response.status
            body = response.read(1024 * 1024)
    except urllib.error.HTTPError as error:
        status = error.code
        body = b""
    if status != expected:
        raise RuntimeError(f"HTTP check failed: {method} {url} returned {status}")
    if require_json and not isinstance(json.loads(body), dict):
        raise RuntimeError("Config endpoint did not return an object")


def public_origin(url):
    """Validate the public page URL and return its origin for API checks."""
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "https" or not parts.hostname or parts.username or parts.password \
            or parts.query or parts.fragment:
        raise ValueError("Public URL must be an https URL without credentials, query or fragment")
    return f"https://{parts.netloc}"


def healthy(public_url=PUBLIC_URL):
    origin = public_origin(public_url)
    for attempt in range(15):
        try:
            http_check("http://127.0.0.1:8787/", 200)
            http_check("http://127.0.0.1:8787/api/config", 200, require_json=True)
            http_check("http://127.0.0.1:8787/api/sessions", 401, method="POST")
            http_check(public_url, 200)
            http_check(f"{origin}/api/config", 200, require_json=True)
            http_check(f"{origin}/api/sessions", 401, method="POST")
            return
        except (OSError, ValueError, RuntimeError):
            if attempt == 14:
                raise RuntimeError("Local/public HTTP or authentication health check failed") from None
            time.sleep(2)


def ensure_rollback_image(previous, run, docker):
    """The image being replaced must be available locally, or rollback is impossible."""
    try:
        run(["docker", "image", "inspect", previous])
        return
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
        pass
    try:
        run(docker + ["pull", previous])
        run(["docker", "image", "inspect", previous])
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
        raise RuntimeError(
            f"Rollback image {previous} is not available locally and could not be pulled; "
            "refusing to deploy without a working rollback") from None


def deploy(payload, stack=STACK, run=command, health=None, public_url=PUBLIC_URL):
    """Deploy one validated release; retain backups and roll back failed changes.

    The SSH caller supplies image digest, matching revision, release branch and
    Actions run identity. Only image selection changes; credentials, data mounts
    and runtime options remain in the server-owned Compose and environment files.
    A revision mismatch or concurrent config change aborts before the restart.
    """
    # 1. Validate the request and current Compose before authenticating or writing.
    validate(payload)
    public_origin(public_url)
    if health is None:
        health = lambda: healthy(public_url)  # noqa: E731
    compose = stack / "compose.yaml"
    original = compose.read_text()
    updated, previous = replace_image(original, payload["image"])
    compose_cmd = ["docker", "compose", "-f", str(compose)]

    # 2. Pull with a job-scoped token, then remove its temporary Docker config.
    #    First make sure the current (rollback) image exists locally.
    with tempfile.TemporaryDirectory(prefix="phone-use-ghcr-") as config:
        docker = ["docker", "--config", config]
        run(docker + ["login", "ghcr.io", "--username", payload["registryUser"],
                      "--password-stdin"], input=payload["registryToken"])
        ensure_rollback_image(previous, run, docker)
        run(docker + ["pull", payload["image"]])
    image = json.loads(run(["docker", "image", "inspect", payload["image"]]))[0]
    if image["Config"].get("Labels", {}).get("org.opencontainers.image.revision") != payload["revision"]:
        raise RuntimeError("Image revision does not match the requested commit")
    if compose.read_text() != original:
        raise RuntimeError("Compose changed during image pull; refusing to overwrite it")

    # 3. Preserve the previous configuration and record before replacing the image.
    history = stack / "ci-deployments" / f'{payload["runId"]}-{payload["attempt"]}'
    history.parent.mkdir(mode=0o700, exist_ok=True)
    history.mkdir(mode=0o700)
    atomic_write(history / "compose.before.yaml", original)
    record_path = stack / "deployment.json"
    if record_path.exists():
        shutil.copy2(record_path, history / "deployment.before.json")
        (history / "deployment.before.json").chmod(0o600)

    def restart():
        run(compose_cmd + ["config", "--quiet"])
        run(compose_cmd + ["up", "-d", "--no-build", "--pull", "never", "control"])

    # 4. Restart, verify the exact running image, and check HTTP/authentication.
    try:
        atomic_write(compose, updated)
        restart()
        container = run(compose_cmd + ["ps", "-q", "control"]).strip()
        if not container or "\n" in container:
            raise RuntimeError("Expected one running control container")
        current = json.loads(run(["docker", "inspect", container]))[0]
        if current["Image"] != image["Id"] or not current["State"]["Running"]:
            raise RuntimeError("Running container does not match the release image")
        health()
        record = {
            "image": payload["image"], "imageDigest": image["Id"],
            "sourceCommit": payload["revision"], "sourceBranch": payload["branch"],
            "runId": payload["runId"], "runAttempt": payload["attempt"],
            "deployedAt": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "previousImage": previous, "rollbackCompose": str(history / "compose.before.yaml"),
            "publicUrl": public_url, "runtimeVerification": "local/public HTTP and authentication passed",
            "realDeviceVerification": "not run by CI",
        }
        content = json.dumps(record, indent=2) + "\n"
        atomic_write(history / "deployment.json", content)
        atomic_write(record_path, content)
        return record
    except Exception:
        # 5. Restore and verify the previous service; report rollback failure too.
        atomic_write(compose, original)
        try:
            restart()
            health()
        except Exception:
            raise RuntimeError(f"Deployment failed; rollback also failed. Backups: {history}") from None
        raise RuntimeError(f"Deployment failed and was rolled back. Backups: {history}") from None


def parse_args(argv=None, environ=os.environ):
    parser = argparse.ArgumentParser(description="Deploy one release image read from stdin.")
    parser.add_argument("--public-url", default=environ.get(PUBLIC_URL_ENV) or PUBLIC_URL,
                        help=f"public page checked after restart (env {PUBLIC_URL_ENV}; default {PUBLIC_URL})")
    args = parser.parse_args(argv)
    public_origin(args.public_url)
    return args


def main():
    args = parse_args()
    os.umask(0o077)
    if os.geteuid() != 0:
        raise RuntimeError("Deployment entry point must run as root")
    raw = sys.stdin.read(16385)
    if len(raw) > 16384:
        raise ValueError("Request too large")
    payload = json.loads(raw)
    validate(payload)
    with (STACK / ".ci-deploy.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        result = deploy(payload, public_url=args.public_url)
    print(json.dumps(result))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Do not print payload, subprocess output, environment, or exception values.
        # Error type + our own RuntimeError/ValueError messages are safe metadata.
        detail = str(error) if type(error) in (RuntimeError, ValueError, BlockingIOError) else type(error).__name__
        print(f"Deployment rejected or failed: {detail}", file=sys.stderr)
        sys.exit(1)
