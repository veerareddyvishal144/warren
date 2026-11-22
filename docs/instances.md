# Instances

Everything about the persistent computers: states, every action with a copy-pasteable call, how sleep works in this build, shapes, and the data that survives what.

Setup used by every example on this page:

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
JSON='Content-Type: application/json'
ID=ab12cd34ef
```

## The object

```json
{
  "id": "ab12cd34ef", "workspace_id": "ws_...", "name": "Production agent",
  "template": "warren-agent", "template_name": "warren-agent", "template_revision": 1,
  "image_ref": "warren/agent:dev", "type": "default", "status": "running", "status_reason": "",
  "resources": { "cpu": 2, "memory": 4, "disk": 4 }, "region": "local",
  "metadata": { "plan": "pro" }, "env": { "APP_ENV": "production" },
  "network": "allow-all", "auto_sleep": true, "idle_timeout_seconds": 900,
  "url": "http://ab12cd34ef.warren.localhost:7337", "default_port": 3737, "mount_path": "/home/agent", "agent_api": true,
  "parent_id": "", "created_at": 1763000000, "updated_at": 1763000100, "started_at": 1763000050,
  "stopped_at": 0, "slept_at": 0, "last_activity_at": 1763000100,
  "runtime": "docker", "disk_used_bytes": 123456
}
```

Timestamps are unix seconds; zero-valued ones are omitted. `url` is derived from the base domain. `runtime` is `docker`, `firecracker` or `lume` (type `mac`). `parent_id` is set on forks. `status_reason` explains `failed`.

## Lifecycle

```
                     create                      stop
  (none) ----------------------> provisioning     +--------> stopping ----> stopped
                                     |            |                           |
                                     v            |                 start     |
                 +---------------> running <------+--------------- starting <-+
                 |                 |  |  |                                     ^
        wake on  |     sleep or    |  |  |  restart / resize / update          |
        request  |  idle timeout   |  |  +------> restarting | updating -------+ (back to running)
                 |                 v  |
              waking <---------- sleeping
                                      |
                              delete  v
                 any state -------> deleting ----> deleted
```

| Status | Meaning | Holds | Counts as awake |
|---|---|---|---|
| `provisioning` | Disk and runtime object being created, image pulled, first start | CPU, memory, disk | yes |
| `running` | The gateway answered its health check | CPU, memory, disk | yes |
| `stopping`, `starting`, `restarting`, `updating` | Transitions you asked for | CPU, memory, disk | yes |
| `stopped` | Container gone, disk kept; never wakes on request | disk only | no |
| `sleeping` | Container or VM gone, disk kept (and the saved memory for `mac` and `vm`); wakes on request | disk only (plus the memory file for `vm` and `mac`) | no |
| `waking` | A request or exec is bringing it back | CPU, memory, disk | yes |
| `failed` | The runtime could not start or run it; see `status_reason` | disk only | no |
| `deleting`, `deleted` | Being destroyed, then gone | nothing | no |

Every action returns immediately with the transitional status; poll `GET /v1/instances/{id}` for the final one. Lists are newest first. "Counts as awake" is what the optional awake quota counts ([quotas.md](quotas.md)).

## Create

```bash
curl -s -X POST $WARREN_API/instances -H "$AUTH" -H "$JSON" -d '{
  "template": "warren-base",
  "type": "default",
  "name": "builder",
  "resources": { "cpu": 4, "memory": 8, "disk": 10 },
  "env": { "GIT_AUTHOR_NAME": "bot", "TOKEN": "..." },
  "metadata": { "customer": "acme", "plan": "pro" },
  "network": "allow-all",
  "auto_sleep": true,
  "idle_timeout_seconds": 1800
}'
```

All fields are optional. Validation, in order: the template must resolve (`name` or `name@rev`); the type must be allowed by the template and have a runtime (`performance`, `vm` without Firecracker and `mac` without Lume answer `400 not_supported`); the shape is normalised (see below); `idle_timeout_seconds` must be within `[300, 86400]` (`[5, 86400]` in dev mode); env keys match `[A-Z_][A-Z0-9_]*`, at most 64 entries, values at most 4 KiB; at most 32 metadata entries; names at most 100 characters. Then the awake quota, when the daemon has one: `409 instance_limit_reached` if the workspace is already at `--max-awake-instances`. For type `mac` the host's capacity is checked too: a Mac runs at most two macOS VMs, and a full host answers `409 host_capacity_reached` with `details: { "max_running": 2, "running": 2 }`. The response is `201` with `status: provisioning`.

Template default env (for example `WARREN_AGENT_DRIVER=openai` on `warren-agent`) is merged under the instance env; the platform variables (`WARREN_INSTANCE_ID`, `WARREN_HOME`, ...) always win.

## Read, list, patch

```bash
curl -s $WARREN_API/instances -H "$AUTH"                      # {"data":[...]} newest first
curl -s $WARREN_API/instances/$ID -H "$AUTH"
curl -s -X PATCH $WARREN_API/instances/$ID -H "$AUTH" -H "$JSON" -d '{
  "name": "builder-2",
  "auto_sleep": false,
  "idle_timeout_seconds": 3600,
  "metadata": { "customer": "acme" },
  "env": { "GIT_AUTHOR_NAME": "bot" },
  "network": "deny-all"
}'
```

Absent fields are unchanged; `metadata`, `env`, `allowed_domains` and `secrets` replace the whole set. Auto-sleep, idle timeout, name, metadata, `allowed_domains` and the values of secrets that keep their name change live; a `network` change recreates the runtime object (a running instance is stopped and started again); `env` and new secret names apply at the next recreate. Unknown fields are rejected with `400 invalid_request`.

## Stop, start, restart

```bash
curl -s -X POST $WARREN_API/instances/$ID/stop -H "$AUTH"      # {"id":"ab12cd34ef","status":"stopping"}
curl -s -X POST $WARREN_API/instances/$ID/start -H "$AUTH"     # {"id":"ab12cd34ef","status":"starting"}
curl -s -X POST $WARREN_API/instances/$ID/restart -H "$AUTH"   # {"id":"ab12cd34ef","status":"restarting"}
```

A stopped instance keeps its disk and never wakes on request: its URLs answer `409 conflict`, exec answers `409`. `start` works from `stopped` and `sleeping` and enforces the awake quota. Processes get 10 seconds to exit on stop before they are killed.

## Sleep and wake: the semantics in this build

```bash
curl -s -X POST $WARREN_API/instances/$ID/sleep -H "$AUTH"     # {"id":"ab12cd34ef","status":"sleeping"}
```

- **Sleep keeps the disk, and the memory where the runtime can.** On type `default` the container is stopped and the files under the mount path are intact; processes and memory are gone, unless the daemon runs with `--docker-checkpoints` on a Docker engine in experimental mode, where sleep checkpoints the container through gVisor (`runsc checkpoint`) and the wake restores it with every process and pid intact ([self-hosting.md](self-hosting.md#sleep-with-memory-on-default-instances)). The instance reports `sleeping` and holds no CPU or memory either way. Types `mac` and `vm` always keep the memory: `mac` suspends the VM through Lume ([macos.md](macos.md)); `vm` pauses the microVM and writes a Firecracker snapshot next to its disk (a memory file as large as the guest's RAM), and the wake resumes it with every process and pid intact ([vm-instances.md](vm-instances.md#sleep-with-memory)).
- **Anything that reaches the instance wakes it**: a request to any of its URLs (default port or `<id>-<port>`), an exec through the Sandbox API, a cron firing. The edge holds the request while the lifecycle starts the container and waits for the gateway's health endpoint, then forwards it. Expect a few seconds; harness clients should allow a generous timeout on the first request. A wake enforces the awake quota like a start (and, for `mac`, the host's two-VM capacity).
- **Auto-sleep** (`auto_sleep: true`) sleeps the instance once `idle_timeout_seconds` pass without activity. Activity is any proxied request or response through the edge and any exec; `last_activity_at` on the object shows the last one. The minimum timeout is 300 seconds (5 in dev mode), the maximum 86,400, the default 900. Default `auto_sleep` is `false`: an instance you create runs until you stop, sleep or delete it.
- **Manual sleep** works regardless of `auto_sleep`.
- **Memory checkpoints on `default`** need `--docker-checkpoints` and an engine in experimental mode; without them a wake starts the container fresh. The checkpoint is single use (the restore removes it) and a stop, resize, update or delete discards it; a restore the engine refuses falls back to a cold start with a warning in the daemon log, which is what current Docker (29.x) does on every wake because its restore is broken ([self-hosting.md](self-hosting.md#sleep-with-memory-on-default-instances)). `vm` keeps memory through Firecracker snapshots and `mac` through Lume suspend. The API is the same on every type.

Design a long-running service inside an instance accordingly: start it from something the first request can trigger, or keep the instance awake with `auto_sleep: false`.

## Exec, logs, metrics

```bash
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H "$JSON" \
  -d '{"command":"df -h /home/agent && ps aux | head","user":"root","timeout_seconds":60}'
curl -s "$WARREN_API/instances/$ID/logs?tail=200" -H "$AUTH"                 # {"logs":"..."} container or VM console
curl -s "$WARREN_API/instances/$ID/logs?since=1763000000" -H "$AUTH"
curl -s "$WARREN_API/instances/$ID/metrics?hours=1" -H "$AUTH"               # {"data":[{"at","cpu_percent","memory_bytes","disk_bytes"}]}
```

Metrics are sampled every minute while running. Exec details are in [exec.md](exec.md).

## Shapes

Shapes are four CPU/RAM pairs; disk is any whole number of GB in the shape's range:

| Shape | Default disk | Disk range, `default` | Disk range, `vm` |
|---|---|---|---|
| 2 vCPU / 4 GB | 4 GB | 2 - 12 GB | 2 - 100 GB |
| 4 vCPU / 8 GB | 6 GB | 2 - 20 GB | 2 - 100 GB |
| 4 vCPU / 16 GB | 12 GB | 2 - 40 GB | 2 - 100 GB |
| 4 vCPU / 32 GB | 24 GB | 2 - 80 GB | 2 - 100 GB |

Type `mac` does not use the per-shape disk ranges: the disk starts at the base image's size, only grows, and may be set up to 500 GB (`warren-macos` defaults to 2 vCPU / 4 GB / 50 GB). CPU and memory limits are enforced per container (or per microVM) from the shape. Disk is a sizing number: the bind-mounted home has no filesystem quota in this build, so watch `disk_used_bytes`.

Normalisation rules (`warren_core::shapes::normalize`): a missing `resources` gives the template's default shape; a CPU count above 4 is clamped to 4 while keeping the requested memory (so `{"cpu": 16, "memory": 32}` becomes 4/32 with 24 GB of disk); an unsupported pair such as 2/8 is `400 invalid_request`; changing the pair without naming a disk picks that pair's default disk; a disk outside the range is `400`.

## Resize

```bash
curl -s -X POST $WARREN_API/instances/$ID/resize -H "$AUTH" -H "$JSON" -d '{"cpu":4,"memory":8}'
# {"id":"ab12cd34ef","status":"running","resources":{"cpu":4,"memory":8,"disk":10}}
curl -s -X POST $WARREN_API/instances/$ID/resize -H "$AUTH" -H "$JSON" -d '{"disk":20}'
```

Name at least one of `cpu`, `memory`, `disk`. Disk may only grow. The runtime applies CPU and memory live when it can; otherwise the instance is stopped, resized and started (`status` goes through `updating`).

## Update (change template or revision)

```bash
curl -s -X POST $WARREN_API/instances/$ID/update -H "$AUTH" -H "$JSON" -d '{"template":"my-agent@3"}'
# {"id":"ab12cd34ef","status":"updating","image_ref":"ghcr.io/acme/agent:1.3","template_revision":3}
curl -s -X POST $WARREN_API/instances/$ID/update -H "$AUTH"      # same template, latest revision
```

The image is replaced and the data under `mount_path` kept. The template's `default_port`, `mount_path` and `agent` flag come from the new template. Not available for type `mac` in this phase (`400 not_supported`): the VM disk is the home, so there is no image to swap underneath it.

## Fork

```bash
curl -s -X POST $WARREN_API/instances/$ID/fork -H "$AUTH" -H "$JSON" -d '{
  "name": "builder copy", "resources": { "disk": 12 }, "auto_sleep": true, "idle_timeout_seconds": 900, "metadata": { "from": "builder" }
}'
```

`201` with a new instance: a copy of the disk, the same template, revision, type and env, a new id and URL, `parent_id` pointing at the source. The source must be stopped or sleeping; a running source is slept first and woken after the copy, so expect a short interruption. The fork is a new awake instance, so the awake quota applies (and, for `mac`, the host's two-VM capacity).

## Backups and restore

```bash
curl -s -X POST $WARREN_API/instances/$ID/backups -H "$AUTH" -H "$JSON" -d '{"note":"before upgrade"}'
# 201 {"id":"...","instance_id":"ab12cd34ef","workspace_id":"ws_...","size_bytes":1234567,"created_at":1763000000,"note":"before upgrade"}
curl -s $WARREN_API/instances/$ID/backups -H "$AUTH"                         # {"data":[...]}
curl -s -X POST $WARREN_API/instances/$ID/restore -H "$AUTH" -H "$JSON" -d '{"backup_id":"..."}'
```

Backups are taken without stopping the instance and stored as `<data>/backups/<id>/<backupId>.tar.gz` on the host. Restore stops the instance, replaces the disk contents and restarts it if it was running; the response is the instance object. Backups are deleted with the instance.

## Delete

```bash
curl -s -X DELETE $WARREN_API/instances/$ID -H "$AUTH"      # {"id":"ab12cd34ef","status":"deleting"}
```

The runtime object and the disk are destroyed, the backups removed, and the instance disappears from the list.

## Network modes

`allow-all` (default) attaches the container to a bridge with egress; `deny-all` attaches it to an internal bridge with no route out. `allowlist` attaches it to the internal bridge too and routes its HTTP(S) through the daemon's egress broker to `allowed_domains` only; `secrets` give it brokered credentials in any mode (see [API.md](API.md#egress-allowlist-and-secrets) and [security.md](security.md)). Switching with `PATCH` recreates the runtime object. Inbound traffic always comes through the edge; instances never get public IPs.

## What survives what

| | Files under `mount_path` | Processes and memory | Everything else in the filesystem |
|---|---|---|---|
| Restart | kept | lost | kept until recreated |
| Sleep and wake, stop and start | kept | lost | reset to the image |
| Resize | kept | lost if a restart was needed | reset if recreated |
| Update | kept | lost | the new image |
| Fork | copied | not copied | the same image |
| Backup and restore | restored | lost | unchanged |
| Delete | removed | removed | removed |

## Errors you will meet

| Code | HTTP | When |
|---|---|---|
| `invalid_request` | 400 | Bad shape, bad env key, idle timeout out of range, unknown JSON field |
| `not_supported` | 400 | `type: "performance"`, `type: "vm"` without the Firecracker runtime, `type: "mac"` without the Lume runtime, a template update (`recreate`) of a `mac` instance, crons on a non-agent template |
| `not_found` | 404 | Unknown id, or an instance of another workspace |
| `conflict` | 409 | Action not valid in the current state (exec or URL on a stopped instance, fork of a provisioning one) |
| `instance_limit_reached` | 409 | Awake quota hit at create, start, wake or fork; `details` has `limit` and `awake` |
| `host_capacity_reached` | 409 | Type `mac` only: the host already runs two macOS VMs at create, start, wake or fork; `details` has `max_running` and `running`. Sleep or stop one to free a slot |
| `try_again` | 503 | Transient: the instance is mid-transition; retry after a moment |
