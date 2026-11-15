# Billing

Warren is prepaid and metered by the minute. This page has the rates, the formula, the rules that stop you from overspending, and the API calls that show the money.

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
JSON='Content-Type: application/json'
```

## Money

All amounts are integer micro-dollars in fields ending `_micros`: $1 is `1000000`. There are no floats on the wire. `POST /v1/signup` grants `5000000` ($5) once per workspace as a `credit` ledger entry.

## Rates

| Rate | Default | Field in `GET /v1/billing` |
|---|---|---|
| One vCPU for a month | $0.80 | `rates.vcpu_month_micros: 800000` |
| One GB of RAM for a month | $0.70 | `rates.ram_gb_month_micros: 700000` |
| One GB of disk for a month | $0.09 | `rates.disk_gb_month_micros: 90000` |
| `vm` type multiplier on compute | 2.5 | `rates.vm_multiplier: 2.5` |
| `performance` multiplier (type not available) | 4.0 | `rates.performance_multiplier: 4.0` |

A month is 730 hours = 43,800 minutes. This build ships the defaults and has no flag to change them; `rates` in `GET /v1/billing` is authoritative for the daemon you talk to.

```
monthly_running = (cpu x vcpu_month + memory x ram_gb_month) x type_multiplier + disk x disk_gb_month
monthly_disk    = disk x disk_gb_month
per_minute      = monthly / 43,800
```

| Shape | `default` running | per hour | per minute | disk only | `vm` running |
|---|---|---|---|---|---|
| 2 vCPU / 4 GB / 4 GB | $4.76 | $0.00652 | 108.68 micros | $0.36 | $11.36 |
| 4 vCPU / 8 GB / 6 GB | $9.34 | $0.01279 | 213.24 micros | $0.54 | $22.54 |
| 4 vCPU / 16 GB / 12 GB | $15.48 | $0.02121 | 353.42 micros | $1.08 | $37.08 |
| 4 vCPU / 32 GB / 24 GB | $27.76 | $0.03803 | 633.79 micros | $2.16 | $66.16 |

Worked example, 2/4/4: `2 x 0.80 + 4 x 0.70 + 4 x 0.09 = 1.60 + 2.80 + 0.36 = $4.76`. On `vm`: `(1.60 + 2.80) x 2.5 + 0.36 = $11.36`.

## What a minute costs in each state

| State | Charged |
|---|---|
| `provisioning` | nothing, until the instance first reaches `running` (`meter_started_at` is set then) |
| `running`, `starting`, `restarting`, `updating`, `stopping` | the full running rate |
| `sleeping`, `stopped`, `waking` | the disk rate only |
| `failed`, `deleting`, `deleted` | nothing |

The meter ticks every minute (`--meter-interval`, 60 s by default). For every instance it charges the whole minutes elapsed since the instance's `metered_through`, carries fractional micros forward so nothing is lost to rounding, and writes a ledger entry with `kind: compute` or `disk`, negative `micros` and the `minutes` charged. Deleting an instance ends billing; the partial minute since the last tick is not charged.

An agent instance with `auto_sleep: true` that is awake two hours a day on the 2/4/4 shape costs about `60 h x $0.00652 + $0.36 = $0.75` a month. The $5 welcome credit covers about 31 days of a 2/4/4 instance running non-stop, or about 14 months of a sleeping one.

## The one-day rule

Creating or forking an instance requires the wallet to hold one day of the instance's running rate, `monthly_running / 30`: about $0.16 for 2/4/4, $0.93 for 4/32/24, $2.21 for 4/32/24 on `vm`. Otherwise the request fails with `402 insufficient_balance` and `details: { "required_micros", "balance_micros" }`. Starting, waking and resizing do not check the balance; they are stopped by the next rule when the money runs out.

## When the wallet reaches zero

The biller stops every running instance in the workspace with `status_reason: "insufficient_balance"`. They stay `stopped` (billing disk only, which can take the balance slightly negative) until you top up and start them explicitly; they do not wake on request. Sleeping instances are left alone.

## Awake limits

The number of instances awake at once (`provisioning`, `running`, `starting`, `restarting`, `updating`, `stopping`, `waking`) is capped by how much the workspace has ever topped up:

| Cumulative top-ups | Awake limit |
|---|---|
| none (welcome credit only) | 3 |
| any top-up | 10 |
| $50 or more | 50 |
| $250 or more | 200 |
| $1,000 or more | 1,000 |

Create, start, wake and fork answer `409 instance_limit_reached` with `details: { "limit", "awake" }` when the cap is hit. Sleeping and stopped instances never count, so auto-sleep is also how you keep many instances for few slots. `GET /v1/workspace` and `GET /v1/billing` return `awake_limit` and `awake_count`.

## API

```bash
curl -s $WARREN_API/billing -H "$AUTH"
```

```json
{
  "wallet_micros": 4821340, "top_up_total_micros": 0, "awake_limit": 3, "awake_count": 1,
  "rates": { "vcpu_month_micros": 800000, "ram_gb_month_micros": 700000, "disk_gb_month_micros": 90000, "vm_multiplier": 2.5, "performance_multiplier": 4.0 },
  "free_credit_granted": true
}
```

```bash
curl -s -X POST $WARREN_API/billing/top-up -H "$AUTH" -H "$JSON" -d '{"amount_micros": 10000000, "provider": "dev"}'
# {"workspace":{...,"wallet_micros":14821340,"top_up_total_micros":10000000},"entry":{"kind":"topup","micros":10000000,...}}
```

`amount_micros` must be between `5000000` ($5) and `10000000000` ($10,000). `provider` defaults to `dev`, the only provider in this build; it is accepted only when the daemon runs with `--dev` and answers `400 not_supported` otherwise, so a production daemon has no way to add money yet. Stripe is on the roadmap.

```bash
curl -s "$WARREN_API/billing/ledger?limit=100" -H "$AUTH"
```

```json
{ "data": [
  { "id": "...", "workspace_id": "ws_...", "instance_id": "ab12cd34ef", "kind": "compute", "micros": -109, "minutes": 1, "at": 1763000160 },
  { "id": "...", "workspace_id": "ws_...", "instance_id": "cd34ef56ab", "kind": "disk", "micros": -8, "minutes": 1, "at": 1763000160 },
  { "id": "...", "workspace_id": "ws_...", "kind": "topup", "micros": 10000000, "at": 1763000100, "note": "dev top-up" },
  { "id": "...", "workspace_id": "ws_...", "kind": "credit", "micros": 5000000, "at": 1763000000, "note": "welcome credit" }
] }
```

Entries are newest first. `kind` is one of `compute`, `disk`, `topup`, `credit`, `adjustment`; `minutes` may be fractional and is omitted when zero; `instance_id` is omitted on wallet-level entries.

```bash
curl -s $WARREN_API/workspace -H "$AUTH"
# {"id":"ws_...","name":"...","created_at":...,"wallet_micros":...,"top_up_total_micros":...,"free_credit_granted":true,"awake_limit":3,"awake_count":1}
```

## Budgets are not the wallet

`budget` on an instance (`monthly_cap_micros`, `credit_micros`) is a cap for managed services the instance might consume; it is exposed inside the instance as environment variables and is not metered in this build. The wallet is the only thing charged.
