# Concepts

Six nouns, one key, and URLs you can compute in your head.

## Instance

An instance is one persistent computer: a Linux container (type `default`, under gVisor), a Linux microVM (type `vm`, Firecracker) or a macOS virtual machine (type `mac`, Lume on an Apple silicon host; see [macos.md](macos.md)) started from a template image, with a persistent disk mounted at the template's `mount_path` (`/home/agent` for the built-in Linux templates, `/Users/lume` for `warren-macos`). It has a 10-character id such as `ab12cd34ef` that doubles as its DNS label, a shape (`resources`: vCPU, GB of RAM, GB of disk), a `status`, a `network` policy (`allow-all` or `deny-all`), an `env` map injected at start, free-form `metadata`, and auto-sleep settings.

Statuses: `provisioning`, `running`, `stopping`, `stopped`, `starting`, `restarting`, `updating`, `sleeping`, `waking`, `failed`, `deleting`, `deleted`. Only the lifecycle component moves an instance between them; the API returns the new transitional status immediately and the instance catches up in the background. Everything in the filesystem outside the mount path is the image and is reset whenever the container is recreated; everything inside survives sleep, stop, restart, resize and template updates and is copied on fork and backup.

The full object is in [API.md](API.md#the-instance-object); lifecycle details in [instances.md](instances.md).

## Template

A template is a named image plus the facts the platform needs to run it: `image_ref`, `default_port` (what the bare instance URL serves; `3737`, the gateway, for the built-in ones), `mount_path`, whether it serves the Agent API (`agent`), the instance types it allows, a default shape and default `env`.

System templates (`warren-base`, `warren-agent`, `warren-opencode`, `warren-claude`, and `warren-macos` on Mac hosts) are seeded by the daemon and visible to everyone. Workspace templates are registered with `POST /v1/templates` from any image that follows the [image contract](../images/common/README.md); posting the same name again publishes a new revision, and `name@3` pins a revision. Instances remember the template and revision they run and can be moved with `POST /v1/instances/{id}/update`. See [templates.md](templates.md).

## Session and response

These belong to the Agent API served inside an instance. A **response** is one turn: `POST /v1/responses` with an `input` returns a response object with an `id`, a `status` (`completed`, `failed`, `in_progress`, `cancelled`) and `output_text`, or streams it as server-sent events. A **session** is the conversation the turn belongs to: pass `session_id` to continue one, omit it to start a new one. Sessions have names, a message list (`role`, `content`, `at`) and are listed newest first. Both are stored as JSON files under `<home>/.warren/` inside the instance, so they persist with the disk and travel with forks and backups. Crons post a prompt with no session id, so every firing is a fresh conversation. See [agent-api.md](agent-api.md).

## Workspace

A workspace is the tenant: many instances, many API keys, its own templates. `POST /v1/signup` creates one and returns `{ "workspace", "api_key", "key_id" }`; the workspace object is `id`, `name` and `created_at`. Keys are `sk_live_` followed by 40 characters; only their SHA-256 is stored, so the plaintext is shown exactly once. `GET /v1/workspace` returns the workspace with its `awake_limit` and `awake_count`; `GET /v1/api-keys`, `POST /v1/api-keys` and `DELETE /v1/api-keys/{id}` manage keys (deleting the key you are using is allowed).

## Awake quota

The operator may cap how many of a workspace's instances are awake at once with `--max-awake-instances N` on the daemon (`WARREN_MAX_AWAKE_INSTANCES`); unset means unlimited. Awake is every state that holds CPU and memory (`provisioning`, `running`, `starting`, `restarting`, `updating`, `stopping`, `waking`); `sleeping` and `stopped` instances hold only their disk and never count. Create, start, wake and fork answer `409 instance_limit_reached` when the workspace is at the limit, and `GET /v1/workspace` shows `awake_limit` (a number or `null`) next to `awake_count`. Auto-sleep is how you keep many instances for few slots. See [quotas.md](quotas.md).

## Two planes, one key

Warren has two HTTP planes, and the same API key opens both.

| Plane | Host | What it does | Auth |
|---|---|---|---|
| Sandbox API | `api.<base>` (also `<base>/v1/...`) | Workspaces, keys, templates, instances and their actions | `Authorization: Bearer sk_live_...` or `X-Warren-Key: sk_live_...` |
| Instance plane | `<id>.<base>` and `<id>-<port>.<base>` | Whatever runs inside the instance: the gateway (Agent API, files, health) on the default port, your own processes on other ports, ttyd on 7681 | the same key in `X-Warren-Key` or `Authorization: Bearer`, **or** a signed URL (`?warren_token=`) which sets the `warren_session` cookie |

The edge in front of both authenticates every instance request and checks that the instance belongs to the key's workspace (`404 not_found` otherwise), wakes a sleeping instance and holds the request until it answers, strips `X-Warren-Key` and the session cookie before forwarding, and passes `Authorization` through unchanged so your own service inside the instance can use it. Nothing inside an instance ever sees the workspace key; the only secret injected is a per-instance token that the daemon uses for exec on VM instances. The dashboard lives on the bare base host and stores the key in the browser.

## Derivable URLs

No lookups are needed to address anything; every URL follows from the base domain, the public scheme and port, and the instance id:

| URL | Pattern | Local default |
|---|---|---|
| Sandbox API | `<scheme>://api.<base>[:port]/v1` | `http://api.warren.localhost:7337/v1` |
| Instance, default port | `<scheme>://<id>.<base>[:port]/` | `http://ab12cd34ef.warren.localhost:7337/` |
| Instance, any port | `<scheme>://<id>-<port>.<base>[:port]/` | `http://ab12cd34ef-7681.warren.localhost:7337/` |
| Dashboard | `<scheme>://<base>[:port]/` | `http://warren.localhost:7337/` |

The port is omitted when it is the scheme's default (443 for https, 80 for http). The instance object carries its `url` ready-made, and `GET /v1/instances/{id}/signed-url?port=` mints a URL a browser can open. Because ids are 10 lowercase alphanumerics starting with a letter and a port suffix is always `-<digits>`, `<id>-<port>` is never ambiguous. In production you point a wildcard DNS record (`*.warren.example.com`) and a wildcard certificate at the daemon and the same patterns hold; see [urls.md](urls.md) and [self-hosting.md](self-hosting.md).
