# Quotas

Warren has one quota: how many of a workspace's instances may be awake at once. It is optional, set by the operator on the daemon, and the same for every workspace. This page has the flag, what counts, which calls enforce it, how to read it, and how to size a host around it.

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
JSON='Content-Type: application/json'
```

## The flag

```bash
warrend --max-awake-instances 3            # or WARREN_MAX_AWAKE_INSTANCES=3
```

Unset means unlimited: a workspace may keep as many instances awake as the host can run. There is no per-workspace cap on vCPUs, RAM or disk in this build; the shape of each instance is enforced per container (CPU and memory limits from `resources`), and the sum of what is awake is bounded only by the host and this quota.

## What counts as awake

| Status | Counts |
|---|---|
| `provisioning`, `running`, `starting`, `restarting`, `updating`, `stopping` | yes |
| `waking` | yes |
| `sleeping`, `stopped` | no |
| `failed`, `deleting`, `deleted` | no |

A sleeping or stopped instance holds only its disk, so it is free as far as the quota is concerned. Auto-sleep (`auto_sleep: true` with an `idle_timeout_seconds`) is therefore how a workspace keeps many instances for few slots: they go to sleep on their own and wake when something reaches them.

## Which calls enforce it

| Call | Effect at the limit |
|---|---|
| `POST /v1/instances` (create) | `409 instance_limit_reached`, nothing is created |
| `POST /v1/instances/{id}/start` | `409`, the instance stays `stopped` or `sleeping` |
| Wake: a request to any instance URL, `POST /v1/instances/{id}/exec`, a cron firing | `409`, the instance stays `sleeping`; the request is not forwarded |
| `POST /v1/instances/{id}/fork` | `409`, no copy is made (the fork is a new awake instance) |

The error body names the limit and the current count:

```json
{ "error": { "code": "instance_limit_reached", "message": "...", "details": { "limit": 3, "awake": 3 } } }
```

Stop, sleep, restart, resize, update, delete, backups, logs and metrics never check the quota: an instance that is already awake may do anything, and freeing a slot always works.

## Reading it

```bash
curl -s $WARREN_API/workspace -H "$AUTH"
```

```json
{ "id": "ws_k3j2h4g5f6d7s8a9q0w1", "name": "my workspace", "created_at": 1763000000, "awake_limit": 3, "awake_count": 1 }
```

`awake_limit` is the daemon's `--max-awake-instances`, or `null` when the daemon has none. `awake_count` is the number of this workspace's instances in an awake state at the moment of the request. The pair is the thing to check when a create or start answers `409 instance_limit_reached`.

## A walk through the limit

With the daemon started as `warrend --dev --data-dir ./data --max-awake-instances 2`:

```bash
A=$(curl -s -X POST $WARREN_API/instances -H "$AUTH" -H "$JSON" -d '{"name":"a"}' | jq -r .id)
B=$(curl -s -X POST $WARREN_API/instances -H "$AUTH" -H "$JSON" -d '{"name":"b"}' | jq -r .id)
curl -s -X POST $WARREN_API/instances -H "$AUTH" -H "$JSON" -d '{"name":"c"}'
# {"error":{"code":"instance_limit_reached","message":"...","details":{"limit":2,"awake":2}}}

curl -s -X POST $WARREN_API/instances/$A/sleep -H "$AUTH"               # frees a slot
curl -s $WARREN_API/workspace -H "$AUTH" | jq '{awake_limit, awake_count}'   # {"awake_limit":2,"awake_count":1}
C=$(curl -s -X POST $WARREN_API/instances -H "$AUTH" -H "$JSON" -d '{"name":"c"}' | jq -r .id)   # 201 now

curl -s http://$A.warren.localhost:7337/v1/health -H "X-Warren-Key: $WARREN_API_KEY"
# 409 instance_limit_reached: waking A would make three awake
curl -s -X DELETE $WARREN_API/instances/$C -H "$AUTH"
curl -s http://$A.warren.localhost:7337/v1/health -H "X-Warren-Key: $WARREN_API_KEY"   # held while A wakes, then 200
```

## Sizing a host

Every awake instance holds its full shape, so add up the vCPUs and RAM of the instances you want awake at once, plus about 1 GB for Docker and the daemon, and set the quota so that the workspaces you serve cannot exceed it together:

| Shape (vCPU / GB RAM) | Default disk | Disk range, `default` | Disk range, `vm` |
|---|---|---|---|
| 2 / 4 | 4 GB | 2 - 12 GB | 2 - 100 GB |
| 4 / 8 | 6 GB | 2 - 20 GB | 2 - 100 GB |
| 4 / 16 | 12 GB | 2 - 40 GB | 2 - 100 GB |
| 4 / 32 | 24 GB | 2 - 80 GB | 2 - 100 GB |

A 16-core, 64 GB host comfortably runs about twelve 2/4 instances or six 4/8 instances awake at once; with ten workspaces, `--max-awake-instances 3` keeps any one of them to a quarter of the machine while auto-sleep lets each own far more instances than that. Disk is the other dimension: sleeping instances still occupy their disk under `<data>/instances/<id>/home`, the disk size is a sizing number without a filesystem quota in this build, so watch `disk_used_bytes` on the instance object and free space on the data volume (see [security.md](security.md#known-limitations)).

## For hosted operators

Warren itself does not account for usage; what it gives you is the data to do it yourself. `GET /v1/instances` lists every instance of a workspace with `status`, `resources`, `type`, `started_at`, `stopped_at`, `slept_at` and `last_activity_at`; `GET /v1/instances/{id}/metrics?hours=` has per-minute CPU, memory and disk samples while running. Poll those from your own service, or wrap `POST /v1/signup` behind your own flow with `--signup-enabled false` and create workspaces from there.
