# Templates

A template is a named image plus what the platform needs to run it. Three come built in; you register your own from any image that follows the image contract.

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

All three serve the gateway on port 3737 (`default_port`), mount the persistent disk at `/home/agent`, allow the `default` and `vm` types, and run as user `agent` (uid 1000) with passwordless sudo. `<tag>` is the daemon's `--image-tag` (`dev` by default) and must match what `scripts/build-images.sh` built.

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
- `default_port` 0 means the bare instance URL serves nothing (`502` with a plain-text explanation); other ports still work at `<id>-<port>`.
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

## Writing an image

The contract is spelled out in [images/common/README.md](../images/common/README.md). The short version: derive from the base image and keep its conventions.

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

An image that does not ship the gateway can still be registered (`agent: false`, `default_port` of its own service); you lose the files API, exec on `vm` instances, the health-based readiness check and the terminal, so this is only useful for the `default` type and plain services.

Platform variables injected into every instance, which always win over template and instance env: `WARREN_INSTANCE_ID`, `WARREN_INSTANCE_TOKEN`, `WARREN_WORKSPACE_ID`, `WARREN_HOME`, `WARREN_GATEWAY_PORT`, `WARREN_BUDGET_MONTHLY_CAP_MICROS`, `WARREN_BUDGET_CREDIT_MICROS`.

## Building the built-in images

```bash
make images                                        # all three, tag dev
WARREN_IMAGE_TAG=v1 scripts/build-images.sh base agent
WARREN_DOCKER_BUILD_ARGS="--pull --no-cache" scripts/build-images.sh    # refresh base layers and "latest" downloads
```

The script builds `warren-gateway` in release mode, stages it at `images/.build/warren-gateway`, runs `docker build` with `images/` as the context and prints the sizes; `warren/base` has a 700 MB budget. Start the daemon with the same tag (`--image-tag v1` or `WARREN_IMAGE_TAG=v1`).
