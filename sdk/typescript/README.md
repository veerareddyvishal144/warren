# Warren TypeScript client

`warren.ts` is a one-file, dependency-free client for the Warren **Sandbox
API** (instances, templates, billing, backups, crons) and the **Agent API**
served inside every instance (chat with streaming, sessions, files, exec). It
is built on `fetch` and `ReadableStream`, so it runs unchanged in Node 20+,
Deno, Bun and browsers.

Use it by copying the file into your project (it is plain TypeScript with only
erasable syntax, so Node 22.6+/24 run it directly with type stripping and any
bundler or `tsx` handles it too), or depend on this directory:

```ts
import { Warren, Agent, WarrenError } from "./warren.ts";
```

## Quick start

```ts
const w = new Warren("http://api.warren.localhost:7337", "sk_live_...");

let inst = await w.createInstance({ template: "warren-agent", auto_sleep: true });
inst = await w.waitFor(inst.id, "running");          // create returns "provisioning" immediately
console.log(inst.url);                                // http://ab12cd34ef.warren.localhost:7337

const result = await w.exec(inst.id, "uname -a");
console.log(result.exit_code, result.stdout);

const agent = w.agent(inst);                          // Agent client for the instance URL, same key
for await (const ev of agent.stream("Summarise the files in my home directory")) {
  if (ev.event === "response.output_text.delta") process.stdout.write(ev.data.delta);
  else if (ev.event === "response.completed") console.log("\n[done, session", ev.data.session_id + "]");
}

await agent.writeFile("notes/todo.md", "- ship it\n");
console.log(await agent.listFiles("notes"));
```

No key yet? `await new Warren(apiUrl).signup({ name: "me" })` returns a
workspace, the plaintext `api_key` (shown once) and the free credit.

## Authentication

* Sandbox API calls send `Authorization: Bearer <key>`.
* Calls to an instance URL send the same key as `x-warren-key`; the edge
  authenticates the request, wakes a sleeping instance and proxies it.
* `agent.exec(token, ...)` additionally sends `x-warren-instance-token`; the
  token is the per-instance secret only the daemon knows (customers use
  `warren.exec` instead).

## Errors

Every failure rejects with `WarrenError` (`code`, `message`, `status`,
`details`):

```ts
try {
  await w.startInstance("ab12cd34ef");
} catch (e) {
  if (e instanceof WarrenError && e.code === "instance_limit_reached") {
    console.log(e.message, e.details);   // { limit: 3, awake: 3 }
  } else throw e;
}
```

`code` is one of `invalid_request`, `invalid_api_key`, `insufficient_balance`,
`forbidden`, `not_found`, `conflict`, `instance_limit_reached`,
`not_supported`, `try_again`, `timeout`, `bad_gateway`, `internal`. `status`
is the HTTP status, or 0 when the request never completed (network failure,
timeout, client-side validation). Aborts through your own `AbortSignal` are
rethrown unchanged.

## Options

```ts
new Warren(apiUrl, apiKey, { timeoutMs: 60_000, fetch: customFetch, headers: { "x-trace": "1" } });
new Agent(instanceUrl, apiKey, { timeoutMs: 600_000 });
await w.getInstance(id, { signal: controller.signal, timeoutMs: 5_000 });   // per call
```

`timeoutMs` bounds a whole call, streams included (0 disables it); `exec`
extends it to cover the command's own `timeout_seconds`. Every method takes
an optional trailing `{ signal, timeoutMs }`.

## Sandbox API (`class Warren`)

| Method | Endpoint |
|---|---|
| `health()`, `version()` | `GET /v1/health`, `GET /v1/version` |
| `signup({ name })` | `POST /v1/signup` |
| `workspace()` | `GET /v1/workspace` |
| `listApiKeys()`, `createApiKey({ name, expires_at })`, `deleteApiKey(id)` | `/v1/api-keys` |
| `billing()`, `topUp({ amount_micros, provider })`, `ledger(limit)` | `/v1/billing`, `/v1/billing/top-up`, `/v1/billing/ledger` |
| `listTemplates()`, `getTemplate(name)`, `registerTemplate(req)`, `deleteTemplate(name)` | `/v1/templates` |
| `createInstance(req)`, `listInstances()`, `getInstance(id)`, `patchInstance(id, req)`, `deleteInstance(id)` | `/v1/instances[/{id}]` |
| `stopInstance`, `startInstance`, `sleepInstance`, `restartInstance` | `POST /v1/instances/{id}/{stop,start,sleep,restart}` |
| `resizeInstance(id, { cpu, memory, disk })`, `updateInstance(id, { template })`, `forkInstance(id, req)` | `POST .../{resize,update,fork}` |
| `exec(id, command \| ExecRequest)` | `POST /v1/instances/{id}/exec` |
| `logs(id, { tail, since })`, `metrics(id, hours)`, `signedUrl(id, { port, ttl, path })` | `GET .../{logs,metrics,signed-url}` |
| `listBackups(id)`, `createBackup(id, { note })`, `restore(id, backupId)` | `.../backups`, `.../restore` |
| `createCron(id, req)`, `listCrons(id)`, `getCron(id, cid)`, `patchCron(id, cid, req)`, `deleteCron(id, cid)` | `.../crons[/{cid}]` |
| `waitFor(id, status, { timeoutMs, pollMs })` | polls `GET /v1/instances/{id}` |
| `instanceUrl(id, port?)`, `agent(instance \| url)` | helpers |

List endpoints resolve with plain arrays (the `{"data": [...]}` envelope is
unwrapped). Request and response shapes are exported as interfaces:
`Instance`, `Template`, `Workspace`, `WorkspaceInfo`, `ApiKey`, `LedgerEntry`,
`Billing`, `Backup`, `Cron`, `MetricPoint`, `ExecRequest`, `ExecResult`,
`CreateInstanceRequest`, `PatchInstanceRequest`, `ForkRequest`,
`RegisterTemplateRequest`, `SignedUrl`, ... Money is in integer micros
(`5_000_000` = $5); timestamps are unix seconds.

## Agent API (`class Agent`)

| Method | Endpoint |
|---|---|
| `health()`, `version()` | `GET /v1/health`, `GET /v1/version` |
| `respond(input \| ResponsesRequest)` | `POST /v1/responses` (waits for the turn) -> `ResponseObject` |
| `stream(input \| ResponsesRequest)` | `POST /v1/responses` with `stream: true`; `AsyncGenerator<StreamEvent>` |
| `getResponse(id)`, `streamResponse(id)`, `cancel(id)` | `/v1/responses/{id}[/stream|/cancel]` |
| `listSessions()`, `getSession(id)`, `renameSession(id, name)`, `deleteSession(id)` | `/v1/sessions[/{id}]` |
| `models()` | `GET /v1/models` |
| `listFiles(path)`, `readFile(path)`, `readTextFile(path)`, `writeFile(path, data, contentType?)`, `deleteFile(path)`, `moveFile(from, to)`, `download(path)` | `/v1/files`, `/v1/files/content`, `/v1/files/move`, `/v1/files/download` |
| `exec(token, command \| ExecRequest)` | `POST /v1/exec` with `x-warren-instance-token` |

`StreamEvent` is a discriminated union, so narrowing on `ev.event` types
`ev.data`: `response.created` `{id, session_id}`, `response.output_text.delta`
`{delta}`, `response.tool_call` `{name, arguments}`, `response.completed`
`{id, session_id, output_text, status}`, `response.failed` `{id, error}`.
Returning early from the `for await` loop closes the connection. Agent API
timestamps are unix milliseconds. The SSE parser is exported too
(`SSEParser`, `parseEventStream`) for other `text/event-stream` endpoints.

## Tests and type-check

Offline tests (a fake `fetch` is injected; nothing is contacted). Node 24
strips types natively; the flag is accepted on Node 22.6+ and harmless where
type stripping is already the default:

```bash
cd sdk/typescript
node --experimental-strip-types --test warren.test.ts     # verified with Node v24.22.3
npx tsx --test warren.test.ts                             # alternative on older Node
npx tsc -p tsconfig.json                                  # type-check (needs the devDependencies)
```
