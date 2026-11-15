# Instances

Everything about the persistent computers: states, every action with a copy-pasteable call, how sleep works in this build, shapes and prices, and the data that survives what.

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
  "budget": { "monthly_cap_micros": 0, "credit_micros": 0 },
  "url": "http://ab12cd34ef.warren.localhost:7337", "default_port": 3737, "mount_path": "/home/agent", "agent_api": true,
  "parent_id": "", "created_at": 1763000000, "updated_at": 1763000100, "started_at": 1763000050,
  "stopped_at": 0, "slept_at": 0, "last_activity_at": 1763000100, "meter_started_at": 1763000050,
  "runtime": "docker", "disk_used_bytes": 123456
}
```

Timestamps are unix seconds; zero-valued ones are omitted. `url` is derived from the base domain. `runtime` is `docker` or `firecracker`. `parent_id` is set on forks. `status_reason` explains `failed` and billing stops (`insufficient_balance`).

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

| Status | Meaning | Bills | Counts as awake |
|---|---|---|---|
| `provisioning` | Disk and runtime object being created, image pulled, first start | nothing until first `running` | yes |
| `running` | The gateway answered its health check | full rate | yes |
| `stopping`, `starting`, `restarting`, `updating` | Transitions you asked for | full rate | yes |
| `stopped` | Container gone, disk kept; never wakes on request | disk only | no |
| `sleeping` | Container gone, disk kept; wakes on request | disk only | no |
| `waking` | A request or exec is bringing it back | disk only | yes |
| `failed` | The runtime could not start or run it; see `status_reason` | nothing | no |
| `deleting`, `deleted` | Being destroyed, then gone | final minutes settled | no |

Every action returns immediately with the transitional status; poll `GET /v1/instances/{id}` for the final one. Lists are newest first.

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
  "idle_timeout_seconds": 1800,
  "budget": { "monthly_cap_micros": 20000000, "credit_micros": 0 }
}'
```

All fields are optional. Validation, in order: the template must resolve (`name` or `name@rev`); the type must be allowed by the template and have a runtime (`performance` and `vm` without Firecracker answer `400 not_supported`); the shape is normalised (see below); `idle_timeout_seconds` must be within `[300, 86400]` (`[5, 86400]` in dev mode); env keys match `[A-Z_][A-Z0-9_]*`, at most 64 entries, values at most 4 KiB; at most 32 metadata entries; names at most 100 characters. Then billing: the wallet must hold one day of the running rate (`402 insufficient_balance`) and the awake limit must allow one more (`409 instance_limit_reached`). The response is `201` with `status: provisioning`. Nothing is charged until the instance first reaches `running`.

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
  "network": "deny-all",
  "budget": { "monthly_cap_micros": 0, "credit_micros": 0 }
}'
```

Absent fields are unchanged; `metadata` and `env` replace the whole map. Auto-sleep, idle timeout, name, metadata, network and budget change live. `env` changes apply at the next start (restart, wake or start). Unknown fields are rejected with `400 invalid_request`.

## Stop, start, restart

```bash
curl -s -X POST $WARREN_API/instances/$ID/stop -H "$AUTH"      # {"id":"ab12cd34ef","status":"stopping"}
curl -s -X POST $WARREN_API/instances/$ID/start -H "$AUTH"     # {"id":"ab12cd34ef","status":"starting"}
curl -s -X POST $WARREN_API/instances/$ID/restart -H "$AUTH"   # {"id":"ab12cd34ef","status":"restarting"}
```

A stopped instance bills disk only and never wakes on request: its URLs answer `409 conflict`, exec answers `409`. `start` works from `stopped` and `sleeping` and enforces the awake limit. Processes get 10 seconds to exit on stop before they are killed.

## Sleep and wake: the semantics in this build

```bash
curl -s -X POST $WARREN_API/instances/$ID/sleep -H "$AUTH"     # {"id":"ab12cd34ef","status":"sleeping"}
```

- **Sleep stops the container and keeps the disk.** Processes and memory are gone; files under the mount path are intact. The instance reports `sleeping` and bills disk only.
- **Anything that reaches the instance wakes it**: a request to any of its URLs (default port or `<id>-<port>`), an exec through the Sandbox API, a cron firing. The edge holds the request while the lifecycle starts the container and waits for the gateway's health endpoint, then forwards it. Expect a few seconds; harness clients should allow a generous timeout on the first request. A wake enforces the awake limit like a start.
- **Auto-sleep** (`auto_sleep: true`) sleeps the instance once `idle_timeout_seconds` pass without activity. Activity is any proxied request or response through the edge and any exec; `last_activity_at` on the object shows the last one. The minimum timeout is 300 seconds (5 in dev mode), the maximum 86,400, the default 900. Default `auto_sleep` is `false`: an instance you create runs until you stop, sleep or delete it.
- **Manual sleep** works regardless of `auto_sleep`.
- **Memory checkpoints** (gVisor checkpoint/restore, Firecracker snapshots) are the roadmap item that will make wake resume processes; the API will not change.

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

## Shapes and prices

Shapes are four CPU/RAM pairs; disk is any whole number of GB in the shape's range. With the default rates ($0.80 per vCPU-month, $0.70 per GB RAM-month, $0.09 per GB disk-month):

| Shape | Default disk | Disk range (`default`) | Running, `default` | Sleeping or stopped | Running, `vm` (compute x 2.5) |
|---|---|---|---|---|---|
| 2 vCPU / 4 GB | 4 GB | 2 - 12 GB | **$4.76 / month** ($0.0065 / hour) | $0.36 / month | $11.36 / month |
| 4 vCPU / 8 GB | 6 GB | 2 - 20 GB | **$9.34 / month** ($0.0128 / hour) | $0.54 / month | $22.54 / month |
| 4 vCPU / 16 GB | 12 GB | 2 - 40 GB | **$15.48 / month** ($0.0212 / hour) | $1.08 / month | $37.08 / month |
| 4 vCPU / 32 GB | 24 GB | 2 - 80 GB | **$27.76 / month** ($0.0380 / hour) | $2.16 / month | $66.16 / month |

Prices with a different disk size follow the formula `cpu x 0.80 + memory x 0.70 + disk x 0.09` (compute part x 2.5 on `vm`), and the `vm` type allows up to 100 GB of disk on every shape. Per-minute charges are monthly / 43,800. The operator's actual rates are in `GET /v1/billing` under `rates`.

Normalisation rules (`warren_core::shapes::normalize`): a missing `resources` gives the template's default shape; a CPU count above 4 is clamped to 4 while keeping the requested memory (so `{"cpu": 16, "memory": 32}` becomes 4/32 with 24 GB of disk); an unsupported pair such as 2/8 is `400 invalid_request`; changing the pair without naming a disk picks that pair's default disk; a disk outside the range is `400`.

## Resize

```bash
curl -s -X POST $WARREN_API/instances/$ID/resize -H "$AUTH" -H "$JSON" -d '{"cpu":4,"memory":8}'
# {"id":"ab12cd34ef","status":"running","resources":{"cpu":4,"memory":8,"disk":10}}
curl -s -X POST $WARREN_API/instances/$ID/resize -H "$AUTH" -H "$JSON" -d '{"disk":20}'
```

Name at least one of `cpu`, `memory`, `disk`. Disk may only grow. The runtime applies CPU and memory live when it can; otherwise the instance is stopped, resized and started (`status` goes through `updating`). Billing follows the new shape from the next metered minute.

## Update (change template or revision)

```bash
curl -s -X POST $WARREN_API/instances/$ID/update -H "$AUTH" -H "$JSON" -d '{"template":"my-agent@3"}'
# {"id":"ab12cd34ef","status":"updating","image_ref":"ghcr.io/acme/agent:1.3","template_revision":3}
curl -s -X POST $WARREN_API/instances/$ID/update -H "$AUTH"      # same template, latest revision
```

The image is replaced and the data under `mount_path` kept. The template's `default_port`, `mount_path` and `agent` flag come from the new template.

## Fork

```bash
curl -s -X POST $WARREN_API/instances/$ID/fork -H "$AUTH" -H "$JSON" -d '{
  "name": "builder copy", "resources": { "disk": 12 }, "auto_sleep": true, "idle_timeout_seconds": 900, "metadata": { "from": "builder" }
}'
```

`201` with a new instance: a copy of the disk, the same template, revision, type and env, a new id and URL, `parent_id` pointing at the source. The source must be stopped or sleeping; a running source is slept first and woken after the copy, so expect a short interruption. The fork is billed as a new instance (one-day rule and awake limit apply).

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

The runtime object and the disk are destroyed, the backups removed, the final minutes settled on the ledger, and the instance disappears from the list.

## Network modes

`allow-all` (default) attaches the container to a bridge with egress; `deny-all` attaches it to an internal bridge with no route out. Switching with `PATCH` applies live. Inbound traffic always comes through the edge; instances never get public IPs. Allowlist egress is a roadmap item.

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
| `not_supported` | 400 | `type: "performance"`, `type: "vm"` without the Firecracker runtime, crons on a non-agent template |
| `insufficient_balance` | 402 | Less than one day of the running rate in the wallet at create or fork; `details` has `required_micros` and `balance_micros` |
| `not_found` | 404 | Unknown id, or an instance of another workspace |
| `conflict` | 409 | Action not valid in the current state (exec or URL on a stopped instance, fork of a provisioning one) |
| `instance_limit_reached` | 409 | Awake limit hit at create, start, wake or fork; `details` has `limit` and `awake` |
| `try_again` | 503 | Transient: the instance is mid-transition; retry after a moment |
