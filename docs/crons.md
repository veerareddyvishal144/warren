# Crons

A cron is a prompt on a schedule. When it fires, Warren wakes the instance if needed and sends the prompt to the agent inside it as a fresh turn. Crons exist only on agent templates (`agent_api: true`); on others every cron call answers `400 not_supported`.

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
JSON='Content-Type: application/json'
ID=ab12cd34ef
```

## Create

```bash
curl -s -X POST $WARREN_API/instances/$ID/crons -H "$AUTH" -H "$JSON" -d '{
  "name": "morning digest",
  "prompt": "Read ~/inbox/*.md, write a one-page digest to ~/digests/$(date +%F).md and delete the processed files.",
  "schedule": "0 7 * * MON-FRI",
  "timezone": "Europe/Berlin",
  "enabled": true
}'
```

```json
{
  "id": "3c9e...", "instance_id": "ab12cd34ef", "workspace_id": "ws_...", "name": "morning digest",
  "prompt": "Read ~/inbox/*.md, ...", "schedule": "0 7 * * MON-FRI", "timezone": "Europe/Berlin", "enabled": true,
  "next_run_at": 1763017200, "created_at": 1763000000, "updated_at": 1763000000
}
```

| Field | Meaning | Limits |
|---|---|---|
| `prompt` | Sent as `input` to `POST /v1/responses` | required; at most 8 KiB |
| `schedule` | Five-field cron expression: minute, hour, day of month, month, day of week | required; see below |
| `timezone` | IANA name the schedule is evaluated in | optional; empty means UTC |
| `name` | A label | optional; at most 100 characters |
| `enabled` | Whether it fires | optional; default `true` |

`last_run_at` and `last_status` appear after the first firing. The response is `201`.

## Schedules

Standard five fields with ranges, lists, steps and day names:

| Expression | Fires |
|---|---|
| `*/15 * * * *` | every 15 minutes |
| `0 * * * *` | on the hour |
| `30 6 * * *` | 06:30 every day |
| `0 9 * * MON-FRI` | 09:00 on weekdays |
| `0 0 1 * *` | midnight on the first of the month |
| `0 22 * * 0,6` | 22:00 on Saturday and Sunday |

Shortcuts such as `@hourly` and six-field (seconds) expressions are rejected with `400 invalid_request`, as is an expression that never fires again. Whitespace is normalised. Times are in `timezone`, so `0 7 * * *` with `Europe/Berlin` fires at 07:00 local time through daylight-saving changes.

## List, read, change, delete

```bash
curl -s $WARREN_API/instances/$ID/crons -H "$AUTH"                     # {"data":[...]} newest first
curl -s $WARREN_API/instances/$ID/crons/3c9e... -H "$AUTH"
curl -s -X PATCH $WARREN_API/instances/$ID/crons/3c9e... -H "$AUTH" -H "$JSON" -d '{"enabled": false}'
curl -s -X PATCH $WARREN_API/instances/$ID/crons/3c9e... -H "$AUTH" -H "$JSON" -d '{"schedule": "0 8 * * MON-FRI", "timezone": "UTC"}'
curl -s -X DELETE $WARREN_API/instances/$ID/crons/3c9e... -H "$AUTH"    # {"id":"3c9e...","deleted":true}
```

`PATCH` accepts any subset of `prompt`, `schedule`, `timezone`, `name`, `enabled`; changing the schedule or timezone recomputes `next_run_at`. Crons are deleted with their instance.

## What happens when it fires

1. The scheduler polls for due crons every 30 seconds, so a firing can be up to 30 seconds late.
2. It wakes the instance if it is `sleeping` (the awake limit applies, like any wake). A `stopped` instance is not started: stopped means "do not run", and the firing is recorded in `last_status` as a conflict.
3. It posts `{"input": "<prompt>"}` to the gateway's `POST /v1/responses` with **no** `session_id`, so every firing is a new conversation with no memory of the previous one beyond what the agent wrote to disk. The scheduler waits up to 10 minutes for the turn.
4. It records `last_run_at` and `last_status` (`ok`, or the error, for example `conflict: instance is stopped`), computes `next_run_at` from the schedule and timezone, and moves on. Firings neither overlap nor queue: while one firing of a cron is still running, the same cron coming due again is skipped until its next scheduled time.

With `auto_sleep: true` the instance goes back to sleep after the idle timeout following the turn. A cron instance that fires once a day on the smallest shape therefore costs roughly the disk rate plus a few running minutes per firing.

## Writing good cron prompts

- Make the work idempotent and file-based: the agent's only memory between firings is the home directory.
- Tell it where to write results (`~/digests/`, `~/reports/latest.md`) so you can fetch them with the files API or a signed URL.
- Keep secrets in the instance `env`, not in the prompt; the prompt is stored in the daemon's database.
