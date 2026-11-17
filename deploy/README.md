# Release branches and VPS deployment

Push a branch named `release-*` containing these workflow files to run Rust checks, browser regressions, deployment tests, a production Docker build and automatic VPS deployment. Main, other branches, PRs and version tags do not trigger app CI or deployment. Documentation publishing is independent.

```bash
# Start from the application version you intend to ship, including the CI changes.
git switch -c release-20261112
git push -u origin release-20261112
```

The CI workflow calls the browser and release reusable workflows. Only after every check succeeds does it build `browser-control/Dockerfile` for Linux amd64 with `WEB_BASE=/iphone/`. It smoke-tests the resulting container before pushing `ghcr.io/trash-boy/phone-use:sha-<commit>` and deploys the immutable `@sha256:...` reference. Never use the unrelated root `docker/Dockerfile` as the web deployment image.

Runs are serialized under `release-vps-production` without cancelling a running deployment. GitHub concurrency retains at most one pending run; a newer push may replace an older pending run. Active runs finish. A server-side file lock also prevents two CI deploy programs from changing the stack simultaneously.

## Provisioned deployment access

Repository Secrets:

| Secret | Purpose |
| --- | --- |
| `VPS_HOST` | VPS SSH hostname/address |
| `VPS_PORT` | SSH port |
| `VPS_USER` | Existing deployment account |
| `VPS_SSH_KEY` | Dedicated ed25519 private key restricted to this deployment entry point |
| `VPS_KNOWN_HOSTS` | Pinned SSH host public key, obtained over the existing trusted connection |

The `vps-production` GitHub environment allows only branch deployments matching `release-*`. Image publishing uses `GITHUB_TOKEN` with `packages: write`; deployment uses a separate job token with `packages: read`. No personal GHCR token is required. The package is linked to this repository through its OCI source label. Keep the package accessible to this repository's Actions token.

The VPS runs a root-owned copy of `deploy-vps.py` at `/usr/local/libexec/phone-use-deploy-vps.py`. The dedicated public key is appended to the deployment user's `authorized_keys` with `restrict` and a forced command:

```text
restrict,command="sudo -n /usr/bin/python3 -I /usr/local/libexec/phone-use-deploy-vps.py" ssh-ed25519 <public-key> phone-use-github-actions-deploy
```

The deployment account must already be permitted to invoke that command through noninteractive sudo. The key does not permit an interactive shell, forwarding, a PTY, SCP or arbitrary remote commands. The entry point reads JSON on stdin and accepts only a `release-*` branch, a commit SHA, an Actions run identity and this repository's image digest. Its GHCR token is used through a temporary Docker config that is removed after pull. Never place real keys, tokens or server environment files in Git.

When updating `deploy-vps.py`, also update the root-owned server copy over the administrator connection after reviewing and testing it. The release job deliberately cannot replace its own privileged entry point.

## Deployment and rollback

The server preserves `/opt/stacks/iphone-control/compose.yaml`, `.env`, environment overrides, the runtime bind mount, Nginx, resource limits and log settings. It replaces only the single control image reference, verifies the OCI revision and running image ID, and checks local/public HTML, config JSON and rejection of unauthenticated session creation. It never creates a phone session or executes phone actions during health checks.

Before restart, it saves the previous Compose and deployment record under `/opt/stacks/iphone-control/ci-deployments/<run-id>-<attempt>/`. Restart or health failure restores the previous Compose and restarts the previous service. Rollback failure is reported as a failure requiring administrator attention. An application restart disconnects active browser/phone sessions; signing state and runtime data remain mounted.

Successful deployment records the source branch, commit, exact image, previous image, timestamp and rollback path in `/opt/stacks/iphone-control/deployment.json`. Actions also prints this record without credentials. Images are not pruned automatically, so the prior image remains available for rollback.

For manual rollback, use the administrator connection and restore the `compose.before.yaml` named by the deployment record, then run:

```bash
sudo docker compose -f /opt/stacks/iphone-control/compose.yaml config --quiet
sudo docker compose -f /opt/stacks/iphone-control/compose.yaml up -d --no-build --pull never control
```

Avoid deploying concurrently through another administrator tool. CI detects Compose changes during its image pull and aborts rather than overwriting them. Manual deployment tooling should honor the same `.ci-deploy.lock` file.

## Claude Code on the VPS

The gateway can use a host Claude Code CLI instead of an OpenAI-compatible endpoint. CC plans one action from the current screenshot and bounded task history. The Rust task loop still owns the phone lease, command validation, confirmation, cancellation, and execution. CC has no built-in tools or native phone socket access. Model errors never fall back to another provider.

`browser-control/cc/bridge.py` runs as the host account already authenticated with CC. It exposes only a mode-0600 Unix socket, under the runtime directory already shared with the container. It does not expose a TCP port or copy the host's credentials into the image. A disconnected planning request terminates its CC child process group. Requests, responses, duration, and concurrency are bounded.

For the existing VPS layout, the host socket is `/srv/containers/iphone-control/cc/bridge.sock`; the container sees `/app/.runtime/cc/bridge.sock`. With `LLM_BACKEND` unset, the gateway selects CC when that socket exists at startup. Set `LLM_BACKEND=cc` to require CC, or `LLM_BACKEND=openai` to explicitly keep the old endpoint. `CC_MODEL` defaults to `sonnet`, and `CC_MODELS` defaults to `opus,sonnet,haiku`. A custom `CC_SOCKET` must be absolute. Restart the gateway after changing backend settings or initially creating the socket. `/api/config` reports the chosen backend and checks bridge health; authentication and model availability require a real planning request to verify.

Start the bridge with an explicit CLI path and a dedicated working directory, for example:

```bash
python3 -I browser-control/cc/bridge.py \
  --socket /srv/containers/iphone-control/cc/bridge.sock \
  --work-dir /srv/containers/iphone-control/cc/work \
  --claude-bin /absolute/path/to/claude
```

The CLI must support print mode, stream-json input/output and the tool/settings isolation flags used by the bridge. It uses the host account's existing CC authentication/provider environment. Authentication injected only into an interactive runner is not automatically available to systemd: verify login in the service account's environment or provide the existing provider settings through the protected service environment file. Never copy runner-internal credentials into that file. No API key belongs in Git or in this command. A successful health response establishes bridge availability; it does not prove model billing/authentication works.

`deploy/phone-use-cc-bridge.service` is an optional systemd **user** service template (no hard-coded account: `%h` is the home of the account that installs it). Copy `bridge.py` to `~/apps/phone-use-cc-bridge/`, the unit to `~/.config/systemd/user/`, then `systemctl --user daemon-reload && systemctl --user enable --now phone-use-cc-bridge` and `loginctl enable-linger "$USER"` so it runs without a login session. The template assumes the CLI at `~/.local/bin/claude`, the optional environment file `~/.config/phone-use-cc-bridge.env` and this VPS's `/srv/containers/iphone-control/cc/` socket/work directory; change any of them with `systemctl --user edit phone-use-cc-bridge` (an empty `ExecStart=` line, then the new one) instead of editing the copied unit. An older system-wide install (`/etc/systemd/system`, `User=`) must be disabled before enabling the user service, since both bind the same socket. Hardening: `NoNewPrivileges=yes` and `MemoryMax=` (3G for CC, 4G for Codex; needs the memory controller delegated to the user manager, the default with cgroup v2). `PrivateTmp=`, `ProtectSystem=strict` and `ReadWritePaths=` are not enabled because user services can only use them through an unprivileged user namespace (implied `PrivateUsers=`), which hosts often restrict, and the unit would then fail to start; the unit comments list a drop-in for hosts where `unshare -Ur true` works. CI does not install services, modify server-owned Compose/environment files, or grant host access. Install and start the host service before the first CC-enabled gateway deployment if using automatic socket detection. Service installation requires administrator approval/access. Removing the socket while leaving an old OpenAI configuration enabled can select OpenAI at the next startup; pin `LLM_BACKEND=cc` in the server environment when CC is required permanently.

System prompts larger than 32 KiB are passed to the CLI through a temporary mode-0600 file in the work directory (`--system-prompt-file`), removed with its process, instead of argv.

### Codex agent planner

`LLM_BACKEND=codex` plans with the host Codex agent instead of a model endpoint. The same `bridge.py` runs with `--codex-bin`. By default (`--codex-mode app-server`) it keeps one long-lived `codex app-server` over stdio JSON-RPC (about 1.9 s per step); `--codex-mode exec` instead runs a `codex exec` turn per step. Either way Codex runs in a read-only sandbox (`approval_policy="never"`) with its tool-providing features disabled, `--codex-model`/`--reasoning-effort` (default low) select the planning model, and the bridge keeps one Codex thread per gateway session, resuming it for later steps and tasks of the same phone connection. The agent's own conversation memory therefore spans the connection; a lost thread falls back to a new one with the full rules. Screenshots are written to a per-request directory under the work directory and removed afterwards. Codex uses its own `~/.codex/config.toml` model and provider; `CODEX_MODELS` (default `default`, meaning the CLI's configured model) and `CODEX_MODEL` select `-m`. The host socket is `/srv/containers/iphone-control/codex/bridge.sock` (container: `/app/.runtime/codex/bridge.sock`, override with `CODEX_SOCKET`). `deploy/phone-use-codex-bridge.service` is the matching systemd user service template (installed the same way, under `~/apps/phone-use-codex-bridge/`). Unlike CC, the Codex backend is never selected automatically: set `LLM_BACKEND=codex` and restart the gateway.

A session's thread is rotated (a fresh thread with the full rules and content) after 30 turns, so accumulated screenshots do not grow its context without bound. With the app-server, a timed-out or disconnected turn is interrupted and the bridge waits up to 3 s for it to end before the thread is used again; if it does not end, the session starts a fresh thread. Only events carrying the current turn's id are taken as its reply. Threads that no session uses any more (closed, evicted beyond 64 sessions, rotated, abandoned, or sessionless) are unsubscribed and archived on the app-server, best effort. The app-server is stopped after 30 minutes without requests and restarted after it has started 200 threads, only while no request uses it; the next request starts a new one, and sessions then begin fresh threads.

#### Bridge HTTP contract

All requests are HTTP/1.1 over the Unix socket, one request per connection, JSON bodies.

- `GET /health` → `200 {"ok":true}`.
- `POST /plan` with `{"model","system","content":[blocks],"session":<id or null>}` → `200 {"reply":{...}}`. Optional fields, ignored when absent:
  - `deadlineMs` (non-negative integer): the caller's remaining budget for this call. The bridge answers within `min(90 s, deadlineMs/1000 − 2 s)`, or `504` at once when less than 1 s would remain. A failed follow-up turn (`502`) falls back to a fresh thread only when at least 20 s of that budget remain; otherwise the `502` is returned. Without `deadlineMs`, each attempt gets the full 90 s, as before.
  - `followUp` (nonempty list of blocks, same shape as `content`): a compact delta (current observation and last step result). Codex sends it instead of `content` when continuing an existing thread; a first turn, a rotated thread and the fresh-thread fallback always use the full `content`. Claude ignores it.
  - Invalid optional fields are rejected with `400`.
- `POST /session/close` with `{"session":"<id>"}` → `200 {"ok":true}`, also for unknown sessions; `400` for a missing or invalid id. Codex forgets the session's thread, lock and turn count, and unloads the thread from the app-server (best effort). Claude keeps no per-session state, so it only answers `200`.
