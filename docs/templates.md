# Templates

A template is a named image plus what the platform needs to run it. Four come built in (five on a Mac host); you register your own from any Linux image: the daemon injects its gateway into images that do not ship it, so files, exec and health work without changing the image (see [Any image works](#any-image-works-the-injected-gateway)).

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
JSON='Content-Type: application/json'
```

## The built-in catalog

| Template | Image | Agent API | Driver | Default shape | What it is |
|---|---|---|---|---|---|
| `warren-base` | `warren/base:<tag>` | no | none | 2 vCPU / 4 GB / 4 GB | A plain Linux computer: Debian 12, Node 24, Python 3, git, build tools, a browser terminal (ttyd on 7681) and the files API. `POST /v1/responses` answers `404 not_supported`. |
| `warren-agent` | `warren/agent:<tag>` | yes | `openai` | 2 vCPU / 4 GB / 4 GB | The built-in agent: an agent loop with shell, read/write-file and list-dir tools on any OpenAI-compatible chat-completions endpoint. Needs `OPENAI_BASE_URL`, `OPENAI_API_KEY` and `WARREN_MODEL` in the instance env. |
| `warren-opencode` | `warren/opencode:<tag>` | yes | `command` | 2 vCPU / 4 GB / 6 GB | OpenCode, the open-source coding agent, run once per turn (`opencode run <prompt>`) in the home directory. Provider keys such as `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` come from the instance env. |
| `warren-claude` | `warren/claude:<tag>` | no | none | 2 vCPU / 4 GB / 6 GB | The plain Linux computer plus the `claude` (Claude Code), `codex` and `opencode` CLIs installed under `/usr/local` for every user. No Agent API: run the CLIs over exec or the browser terminal, with `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and friends in the instance env. The same CLIs go into the macOS base image with `scripts/mac/build-base.sh --with-agents`. |
| `warren-macos` | the base Lume VM named by `--mac-base-vm` | no | none | 2 vCPU / 4 GB / 50 GB | A macOS virtual machine on an Apple silicon host, type `mac` only: shell, exec and the files API inside a full macOS guest. Present in the catalog only when the daemon runs with `--mac-base-vm`. See [macos.md](macos.md). |

The four Linux templates serve the gateway on port 3737 (`default_port`), mount the persistent disk at `/home/agent`, allow the `default` and `vm` types, and run as user `agent` (uid 1000) with passwordless sudo. `<tag>` is the daemon's `--image-tag` (`dev` by default) and must match what `scripts/build-images.sh` built. `warren-macos` also serves the gateway on 3737 but mounts the home at `/Users/lume`, allows only `mac`, and its disk starts at the base image's size and only grows (up to 500 GB).

```bash
curl -s $WARREN_API/templates -H "$AUTH"                 # system catalog first, then workspace templates by name
curl -s $WARREN_API/templates/warren-agent -H "$AUTH"
```

A template object:

```json
{
  "name": "warren-agent", "scope": "system", "image_ref": "warren/agent:dev", "image_digest": "sha256:...",
  "default_port": 3737, "mount_path": "/home/agent", "agent": true,
  "description": "The built-in agent: chat, shell and file tools on any OpenAI-compatible model.",
  "types": ["default", "vm"], "default_shape": { "cpu": 2, "memory": 4, "disk": 4 },
  "env": { "WARREN_AGENT_DRIVER": "openai" }, "revision": 1, "created_at": 1763000000, "updated_at": 1763000000
}
```

Workspace templates carry `"scope": "workspace"` and their `workspace_id`. Empty `types` means every type is allowed.

## Register your own

```bash
curl -s -X POST $WARREN_API/templates -H "$AUTH" -H "$JSON" -d '{
  "name": "my-agent",
  "image_ref": "ghcr.io/acme/my-agent:1.0",
  "default_port": 3737,
  "mount_path": "/home/agent",
  "agent": true,
  "description": "Our support agent",
  "env": { "WARREN_AGENT_DRIVER": "command", "WARREN_AGENT_COMMAND": "my-agent --prompt {prompt}" }
}'
```

- `name`: lowercase letters, digits and dashes, 3 to 40 characters, not starting with `warren-` (reserved for the catalog).
- `image_ref` is required. The daemon pulls the image when you register and records its digest, so a mutable tag is frozen at the revision. A private registry needs the Docker daemon on the host to be logged in.
- `default_port` is where the bare instance URL lands: `3737` makes `https://<id>.<base>/` the gateway (files, health, the Agent API if `agent: true`); the port of your own service makes the bare URL your service while the gateway stays at `https://<id>-3737.<base>/`; `0` means the bare URL serves nothing (`502` with the plain-text explanation "this template has no default port") while `<id>-3737` still reaches the gateway and any other port works at `<id>-<port>`.
- `mount_path` is where the persistent disk mounts; it must be a directory the image's processes can use (`/home` for `alpine:3.20`, `/home/node` for the official Node images, `/home/agent` for images derived from `warren/base`).
- `agent: true` says the image answers `POST /v1/responses` on `default_port`, which also enables crons.

The response is `201` with `"revision": 1`. Posting again with the same name publishes revision 2, 3, ...; older revisions stay readable:

```bash
curl -s $WARREN_API/templates/my-agent -H "$AUTH"          # latest revision
curl -s $WARREN_API/templates/my-agent@1 -H "$AUTH"        # a pinned revision; 404 if unknown or unpublished
```

Instances pin whatever they were created or updated with:

```bash
curl -s -X POST $WARREN_API/instances -H "$AUTH" -H "$JSON" -d '{"template":"my-agent@2","name":"support-7"}'
curl -s -X POST $WARREN_API/instances/ab12cd34ef/update -H "$AUTH" -H "$JSON" -d '{"template":"my-agent@3"}'
curl -s -X POST $WARREN_API/instances/ab12cd34ef/update -H "$AUTH"      # the instance's own template, latest revision
```

Update replaces the image and keeps the data under `mount_path`. A pin on a system template must equal its current revision.

## Delete

```bash
curl -s -X DELETE $WARREN_API/templates/my-agent -H "$AUTH"     # {"name":"my-agent","deleted":true}
```

`409 conflict` while any instance uses the template (any revision). Move or delete those instances first. System templates cannot be deleted.

## Any image works: the injected gateway

A registered image does not have to know about Warren. When an instance of a workspace template starts, the runtime injects the daemon's static `warren-gateway` binary into the container at `/warren/warren-gateway` (outside any home, so it never collides with your files) and runs it as the entrypoint in **wrap mode**: the gateway serves the Warren API on port 3737 and starts the image's own `ENTRYPOINT` and `CMD` as a child process, which it supervises. The decision is by template scope: workspace templates get the gateway injected; the built-in (`system`) templates already contain it at `/usr/local/bin/warren-gateway` and run it directly, unwrapped.

What this gives you on any Linux image, Alpine and distroless ones included (the binary is fully static, built for `x86_64-unknown-linux-musl`, and needs no libc, shell or user database):

- The **files API** on 3737, confined to `mount_path`.
- **Exec** (`POST /v1/instances/{id}/exec`), defaulting to the owner of the persistent home as everywhere.
- **Health**: `GET /v1/health` answers `{"healthy": true, ..., "wrapped": {"running": <bool>, "exit_code": <int|null>}}`, so the daemon's readiness check works and you can see whether the image's own process is still running and how it exited.
- **No long-running process required.** `alpine:3.20` runs `/bin/sh`, which exits at once without a stdin; the gateway reports `"wrapped": {"running": false, "exit_code": 0}` and keeps serving, so the instance stays `running` and usable for exec and files. A service image (nginx, n8n, your own agent) keeps running as the child.
- **Terminal-less operation.** A foreign image has no `ttyd`, so `<id>-7681` answers `502` and the dashboard terminal is unavailable; everything else works. Derive from `warren/base` if you want the browser terminal.
- **The image's user is kept.** An image with `USER node` runs the gateway and its child as `node`; one without `USER` runs as root like the built-in images.

Only workspace templates are wrapped, and only when the daemon has the binary: `--gateway-bin <path>` (or `WARREN_GATEWAY_BIN`), else a `warren-gateway` next to `warrend`, else `/usr/local/bin/warren-gateway`. `make gateway` builds it (`make musl-setup` once for the musl toolchain); `scripts/dev-up.sh` builds it when missing and passes the flag. Without it the daemon warns at startup and instances of registered images that lack a gateway cannot start.

## Writing an image

The contract is spelled out in [images/common/README.md](../images/common/README.md). Deriving from the base image is optional (any image works, see above); it gives you the `agent` user, the toolchain, `ttyd` and the gateway baked in. If you derive, keep its conventions.

```dockerfile
FROM warren/base:dev
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg && rm -rf /var/lib/apt/lists/*
RUN npm install -g my-agent
ENV WARREN_AGENT_DRIVER=command WARREN_AGENT_COMMAND='my-agent --prompt {prompt}'
```

- Keep user `agent` (uid 1000, gid 1000, home `/home/agent`) and do not add a `USER` instruction: the container starts as root and the gateway drops privileges where needed.
- Keep `ENTRYPOINT ["/usr/local/bin/warren-gateway", "--serve"]`, do not add a `CMD`, keep ports 3737 and 7681. The daemon waits for `GET /v1/health` on 3737 before it reports `running`.
- `/home/agent` is replaced by the persistent disk at runtime, so nothing the image puts there is visible. Put defaults in `/etc` (`/etc/profile.d`, `/etc/skel`) and tools in `/usr/local`.
- Never bake credentials in. Keys arrive through the instance `env`; the workspace API key is never present inside an instance.
- The driver is selected by `WARREN_AGENT_DRIVER`: `openai` (agent loop against `OPENAI_BASE_URL` with `OPENAI_API_KEY` and `WARREN_MODEL`), `command` (`WARREN_AGENT_COMMAND` run once per turn, `{prompt}` replaced by the input or appended as the last argument, stdout is the reply), `echo` (deterministic, for tests). Unset means no agent.

The daemon decides by template scope, not by inspecting the image: every workspace template, including one derived from `warren/base` that carries a gateway of its own, starts with the injected gateway as the entrypoint and the image's own entrypoint as its child.

Platform variables injected into every instance, which always win over template and instance env: `WARREN_INSTANCE_ID`, `WARREN_INSTANCE_TOKEN`, `WARREN_WORKSPACE_ID`, `WARREN_HOME`, `WARREN_GATEWAY_PORT`.

## Building the built-in images

```bash
make images                                        # all four, tag dev
WARREN_IMAGE_TAG=v1 scripts/build-images.sh base agent
scripts/build-images.sh claude                     # just warren/claude (needs warren/base of the same tag)
WARREN_DOCKER_BUILD_ARGS="--pull --no-cache" scripts/build-images.sh    # refresh base layers and "latest" downloads
```

The script builds `warren-gateway` in release mode, stages it at `images/.build/warren-gateway`, runs `docker build` with `images/` as the context (base, agent, opencode, claude in that order) and prints the sizes; `warren/base` must stay under 700 MB. `WARREN_OPENCODE_VERSION`, `WARREN_CLAUDE_CODE_VERSION` and `WARREN_CODEX_VERSION` pin the npm packages the opencode and claude images install (default `latest`). Start the daemon with the same tag (`--image-tag v1` or `WARREN_IMAGE_TAG=v1`).
