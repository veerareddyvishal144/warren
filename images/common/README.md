# Warren template images

Every Warren instance is a container (or microVM) started from a template
image. This directory documents the contract those images follow, so that the
built-in images (`images/base`, `images/agent`, `images/opencode`) and any
workspace template registered through `POST /v1/templates` behave the same way
towards the daemon, the edge and the gateway.

## Images

| Image | Template | Driver | Purpose |
|---|---|---|---|
| `warren/base:<tag>` | `warren-base` | none | A plain Linux computer: Node, Python, git, build tools, a browser terminal and the files API. No agent. |
| `warren/agent:<tag>` | `warren-agent` | `openai` | The built-in agent: chat, shell and file tools on any OpenAI-compatible model. |
| `warren/opencode:<tag>` | `warren-opencode` | `command` | OpenCode, the open-source coding agent, behind the Agent API. |

`<tag>` is `WARREN_IMAGE_TAG` (default `dev`); the daemon's `image_tag` config
must match. `warren/agent` and `warren/opencode` derive from `warren/base` of
the same tag. Build all three with `scripts/build-images.sh` (see below).

## The contract

### User and home

- The instance user is `agent`, uid `1000`, gid `1000`, shell `/bin/bash`,
  home `/home/agent`, passwordless sudo (`/etc/sudoers.d/agent`).
- `/home/agent` is the persistent disk. The runtime bind-mounts
  `<DataDir>/instances/<id>/home` (or the VM rootfs equivalent) at the
  template's `mount_path`, which is `/home/agent` for every built-in template.
  Files there survive sleep, stop, restart, resize, template update and are
  copied on fork and backup. Everything else in the filesystem is the image
  and is reset whenever the container is recreated.
- Because the mount replaces the directory, nothing the image puts under
  `/home/agent` is visible at runtime. Put defaults in `/etc` (for example
  `/etc/profile.d`, `/etc/skel`) and tools under `/usr/local`.
- uid 1000 matches the developer account on the dev host, so bind-mounted
  homes are writable without a chown.

### Processes and ports

- The container starts as **root** with
  `ENTRYPOINT ["/usr/local/bin/warren-gateway", "--serve"]`. The gateway is
  the only long-running process the image provides; in microVMs it is PID 1
  (`--init`). It drops to `agent` for ttyd and honours the `user` field of an
  exec request (the image user, `agent`, or `root`).
- **Port 3737** (`WARREN_GATEWAY_PORT`): the gateway serves the Agent API
  (`/v1/responses`, sessions, models), the files API, `/v1/exec` (daemon only,
  requires `X-Warren-Instance-Token`), `/v1/health` and `/v1/version`. It is
  the `default_port` of every built-in template, so the bare instance URL
  `https://<id>.<base>/` lands on it.
- **Port 7681**: `ttyd`, the browser terminal, supervised by the gateway and
  running a login shell as `agent` in `/home/agent`. Reached at
  `https://<id>-7681.<base>/`, usually through a signed URL.
- Any other port a process inside the instance listens on is reachable at
  `https://<id>-<port>.<base>/`; nothing in the image needs to declare it.
- The daemon waits for `GET /v1/health` on 3737 before it reports the instance
  `running`, so the gateway must come up on its own without configuration
  beyond the environment below.

### Environment

The daemon injects these variables into every instance
(`warren_core::types`, `Instance::effective_env`). They always win over
template and instance values:

| Variable | Meaning |
|---|---|
| `WARREN_INSTANCE_ID` | The 10-character instance id. |
| `WARREN_INSTANCE_TOKEN` | Secret the gateway requires on `POST /v1/exec`. Only the daemon knows it. |
| `WARREN_WORKSPACE_ID` | Owning workspace (`ws_...`). |
| `WARREN_HOME` | The persistent home, `/home/agent`; the gateway confines the files API to it and runs agents in it. |
| `WARREN_GATEWAY_PORT` | Port the gateway listens on, `3737`. |

Precedence is template `env` < instance `env` < the platform variables above.
Template-level variables select the agent behaviour:

| Variable | Set by | Meaning |
|---|---|---|
| `WARREN_AGENT_DRIVER` | template | `echo` (tests), `openai` (`warren/agent`), `command` (`warren/opencode`). Unset or empty: no agent, `/v1/responses` answers `404 not_supported`. |
| `WARREN_AGENT_COMMAND` | template | For the `command` driver: the command run once per turn, `{prompt}` replaced by the input (else appended as the last argument). |
| `WARREN_MODEL` | instance | Model name for the `openai` driver (and `GET /v1/models`). |
| `OPENAI_BASE_URL`, `OPENAI_API_KEY` | instance | Endpoint and key for the `openai` driver. |

Images never contain secrets. Keys, endpoints and tokens arrive through the
instance `env` or the platform variables at start time; the workspace API key
is never present inside an instance.

Convenience variables set by `warren/base`: `LANG=C.UTF-8`,
`PIP_BREAK_SYSTEM_PACKAGES=1` (so `pip install` works on Debian's system
Python without a venv) and `PATH` including `/home/agent/.local/bin`.

### Toolchain in `warren/base`

Debian 12 (bookworm-slim) with `ca-certificates curl git python3 python3-pip
python3-venv procps tar unzip jq sudo openssh-client build-essential`, Node 24
with npm and npx (copied from `node:24-bookworm-slim`), `python` as an alias
for `python3`, and `ttyd` (static x86_64 binary of the latest GitHub release,
checksum-verified). `python3-dev` headers are not included; `sudo apt-get
install python3-dev` inside an instance adds them. Global `npm install -g` as
`agent` needs `sudo` (the npm prefix is `/usr/local`); `npm install` in a
project under the home works without it.

The base image must stay under 700 MB; `scripts/build-images.sh` prints the
size of every image it builds and warns when the limit is exceeded.

## Building

```bash
scripts/build-images.sh                 # all three images, tag dev
WARREN_IMAGE_TAG=v1 scripts/build-images.sh base agent
```

The script sources `scripts/env.sh`, builds the gateway in release mode
(`cargo build --release --locked -p warren-gateway`), stages the binary at
`images/.build/warren-gateway` (git-ignored), runs `docker build` for each
image with the `images/` directory as the build context and `warren/<name>:<tag>`
as the tag, and prints the resulting sizes. Set `WARREN_GATEWAY_BIN` to stage a
prebuilt binary instead of compiling, `WARREN_SKIP_GATEWAY_BUILD=1` to reuse
what is already staged, `WARREN_TTYD_VERSION` or `WARREN_OPENCODE_VERSION` to
pin those downloads, and `WARREN_DOCKER_BUILD_ARGS="--pull --no-cache"` to
refresh the base layers and the "latest" downloads.

The gateway binary is a glibc build for Debian bookworm, which is why the
images are not Alpine.

## Writing your own template

Derive from the base image and keep the contract:

```dockerfile
FROM warren/base:dev
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg && rm -rf /var/lib/apt/lists/*
ENV WARREN_AGENT_DRIVER=command WARREN_AGENT_COMMAND='my-agent --prompt {prompt}'
```

- Keep user `agent` (uid 1000) and `/home/agent`; do not add a `USER`
  instruction, the gateway must stay the root entrypoint.
- Keep the `ENTRYPOINT` and ports 3737 and 7681; do not add a `CMD`, it
  would be passed to the gateway as extra arguments.
- Register it with `POST /v1/templates` (`image_ref`, `default_port: 3737`,
  `mount_path: "/home/agent"`, `agent: true` when it answers `/v1/responses`).
  The daemon pulls the image and records its digest.
- Do not store state outside `/home/agent`; do not bake credentials in.
