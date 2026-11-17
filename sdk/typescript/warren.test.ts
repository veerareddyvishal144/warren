/**
 * Offline tests for warren.ts: SSE parsing, error envelopes, URL building,
 * headers and bodies. No sockets are opened; `fetch` is injected.
 *
 * Run:  node --experimental-strip-types --test warren.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { Agent, SSEParser, Warren, WarrenError, codeForStatus, parseEventStream } from "./warren.ts";
import type { SSEEvent, StreamEvent } from "./warren.ts";

const API_URL = "http://api.warren.localhost:7337";
const INSTANCE_URL = "http://ab12cd34ef.warren.localhost:7337";
const KEY = `sk_live_${"A".repeat(40)}`;

interface Call {
  url: string;
  method: string;
  headers: Headers;
  body: unknown;
  signal: AbortSignal | undefined;
}

type Answer = Response | Error | ((call: Call) => Response | Promise<Response>);

/** A fake `fetch` that records calls and replays canned answers. */
function recorder(...answers: Answer[]) {
  const calls: Call[] = [];
  const fetchImpl = (async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    const call: Call = {
      url,
      method: init?.method ?? "GET",
      headers: new Headers(init?.headers),
      body: init?.body,
      signal: init?.signal ?? undefined,
    };
    calls.push(call);
    const answer = answers.shift() ?? json(200, {});
    if (answer instanceof Error) throw answer;
    return typeof answer === "function" ? answer(call) : answer;
  }) as typeof fetch;
  return {
    fetchImpl,
    calls,
    last(): Call {
      const c = calls[calls.length - 1];
      if (!c) throw new Error("no call recorded");
      return c;
    },
  };
}

const json = (status: number, body: unknown): Response =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const text = (status: number, body: string, type = "text/plain"): Response =>
  new Response(body, { status, headers: { "content-type": type } });

function byteStream(chunks: string[], onCancel?: () => void): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
    cancel() {
      onCancel?.();
    },
  });
}

const sse = (chunks: string[]): Response =>
  new Response(byteStream(chunks), { status: 200, headers: { "content-type": "text/event-stream" } });

async function collect<T>(it: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const v of it) out.push(v);
  return out;
}

async function rejects(p: Promise<unknown>): Promise<WarrenError> {
  try {
    await p;
  } catch (e) {
    assert.ok(e instanceof WarrenError, `expected WarrenError, got ${String(e)}`);
    return e;
  }
  assert.fail("expected a rejection");
}

const SAMPLE_STREAM =
  ": keep-alive comment\n" +
  "event: response.created\n" +
  'data: {"id": "r1", "session_id": "s1"}\n' +
  "\n" +
  "event: response.output_text.delta\r\n" +
  'data: {"delta": "Hel"}\r\n' +
  "\r\n" +
  "event: response.output_text.delta\n" +
  'data:{"delta": "lo"}\n' +
  "\n" +
  "data: line one\n" +
  "data: line two\n" +
  "\n" +
  "event: ignored.without.data\n" +
  "\n" +
  "id: 7\n" +
  "retry: 1000\n" +
  "event: response.completed\n" +
  'data: {"id": "r1", "session_id": "s1",\n' +
  'data:  "output_text": "Hello", "status": "completed"}\n' +
  "\n" +
  "event: response.failed\n" +
  "data: never terminated by a blank line\n";

const EXPECTED_RAW: SSEEvent[] = [
  { event: "response.created", data: '{"id": "r1", "session_id": "s1"}' },
  { event: "response.output_text.delta", data: '{"delta": "Hel"}' },
  { event: "response.output_text.delta", data: '{"delta": "lo"}' },
  { event: "message", data: "line one\nline two" },
  { event: "response.completed", data: '{"id": "r1", "session_id": "s1",\n "output_text": "Hello", "status": "completed"}', id: "7" },
];

const EXPECTED_EVENTS = [
  { event: "response.created", data: { id: "r1", session_id: "s1" } },
  { event: "response.output_text.delta", data: { delta: "Hel" } },
  { event: "response.output_text.delta", data: { delta: "lo" } },
  { event: "message", data: "line one\nline two" },
  { event: "response.completed", data: { id: "r1", session_id: "s1", output_text: "Hello", status: "completed" } },
];

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

test("SSEParser parses a whole buffer", () => {
  const parser = new SSEParser();
  assert.deepEqual(parser.feed(SAMPLE_STREAM), EXPECTED_RAW);
});

test("SSEParser is independent of chunk boundaries", () => {
  for (const size of [1, 2, 3, 7, 13, 64]) {
    const parser = new SSEParser();
    const out: SSEEvent[] = [];
    for (let i = 0; i < SAMPLE_STREAM.length; i += size) out.push(...parser.feed(SAMPLE_STREAM.slice(i, i + size)));
    assert.deepEqual(out, EXPECTED_RAW, `chunk size ${size}`);
  }
});

test("SSEParser handles CR, CRLF and a CRLF split across chunks", () => {
  const stream = 'event: a\rdata: {"n": 1}\r\r' + 'event: b\r\ndata: {"n": 2}\r\n\r\n';
  const expected: SSEEvent[] = [
    { event: "a", data: '{"n": 1}' },
    { event: "b", data: '{"n": 2}' },
  ];
  assert.deepEqual(new SSEParser().feed(stream), expected);
  const cut = stream.indexOf("\n");
  const parser = new SSEParser();
  assert.deepEqual([...parser.feed(stream.slice(0, cut)), ...parser.feed(stream.slice(cut))], expected);
});

test("SSEParser ignores comments, drops events without data, defaults the name", () => {
  const parser = new SSEParser();
  assert.deepEqual(parser.feed(": hi\n\nevent: nothing\n\n"), []);
  assert.deepEqual(parser.feed("data: x\n\n"), [{ event: "message", data: "x" }]);
  assert.deepEqual(parser.feed("event: later\ndata: y"), []);
  assert.deepEqual(parser.feed("\n\n"), [{ event: "later", data: "y" }]);
});

test("parseEventStream decodes UTF-8 split across chunks and discards a pending tail", async () => {
  const payload = 'event: response.output_text.delta\ndata: {"delta": "héllo wörld"}\n\nevent: response.failed\ndata: pending';
  const bytes = new TextEncoder().encode(payload);
  const cut = payload.indexOf("é") + 1; // inside the two-byte sequence
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes.slice(0, cut));
      controller.enqueue(bytes.slice(cut));
      controller.close();
    },
  });
  assert.deepEqual(await collect(parseEventStream(stream)), [
    { event: "response.output_text.delta", data: '{"delta": "héllo wörld"}' },
  ]);
});

test("parseEventStream cancels the source when the consumer returns early", async () => {
  let cancelled = false;
  const stream = byteStream(["data: 1\n\ndata: 2\n\n", "data: 3\n\n"], () => {
    cancelled = true;
  });
  for await (const ev of parseEventStream(stream)) {
    assert.equal(ev.data, "1");
    break;
  }
  assert.equal(cancelled, true);
});

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

test("WarrenError.fromBody decodes the envelope, bare objects and plain text", () => {
  const env = WarrenError.fromBody(409, JSON.stringify({ error: { code: "conflict", message: "instance is stopped", details: { status: "stopped" } } }));
  assert.equal(env.code, "conflict");
  assert.equal(env.status, 409);
  assert.equal(env.message, "instance is stopped");
  assert.deepEqual(env.details, { status: "stopped" });
  assert.equal(env.toString(), "conflict (HTTP 409): instance is stopped");
  assert.ok(env instanceof Error);
  assert.equal(env.name, "WarrenError");

  const bare = WarrenError.fromBody(404, JSON.stringify({ code: "not_found", message: "instance not found" }));
  assert.equal(bare.code, "not_found");
  assert.equal(bare.status, 404);

  const plain = WarrenError.fromBody(502, "this template has no default port\n");
  assert.equal(plain.code, "bad_gateway");
  assert.equal(plain.message, "HTTP 502: this template has no default port");

  const odd = WarrenError.fromBody(401, JSON.stringify({ unexpected: true }));
  assert.equal(odd.code, "invalid_api_key");

  const empty = WarrenError.fromBody(418, "");
  assert.equal(empty.code, "internal");
  assert.equal(empty.message, "HTTP 418");
});

test("codeForStatus maps bare statuses", () => {
  assert.equal(codeForStatus(400), "invalid_request");
  assert.equal(codeForStatus(401), "invalid_api_key");
  assert.equal(codeForStatus(403), "forbidden");
  assert.equal(codeForStatus(503), "try_again");
  assert.equal(codeForStatus(504), "timeout");
  assert.equal(codeForStatus(500), "internal");
});

// ---------------------------------------------------------------------------
// Sandbox API
// ---------------------------------------------------------------------------

test("Warren normalises the base URL and rejects bad ones", () => {
  for (const given of [API_URL, `${API_URL}/`, `${API_URL}/v1`, `${API_URL}/v1/`]) {
    assert.equal(new Warren(given, KEY, { fetch: recorder().fetchImpl }).apiUrl, API_URL, given);
  }
  assert.throws(() => new Warren("api.warren.localhost", KEY, { fetch: recorder().fetchImpl }), (e: unknown) => e instanceof WarrenError && e.code === "invalid_request");
});

test("Warren sends Bearer auth and builds instance URLs", async () => {
  const r = recorder(json(200, { id: "ab12cd34ef", status: "running" }));
  const w = new Warren(`${API_URL}/v1`, KEY, { fetch: r.fetchImpl });
  const inst = await w.getInstance("ab12cd34ef");
  assert.equal(inst.status, "running");
  const call = r.last();
  assert.equal(call.url, `${API_URL}/v1/instances/ab12cd34ef`);
  assert.equal(call.method, "GET");
  assert.equal(call.headers.get("authorization"), `Bearer ${KEY}`);
  assert.equal(call.headers.get("accept"), "application/json");
  assert.equal(call.headers.get("x-warren-key"), null);
  assert.equal(call.body, undefined);
  assert.ok(call.signal instanceof AbortSignal);
});

test("createInstance drops undefined fields; actions send no body; optional bodies are objects", async () => {
  const r = recorder(
    json(201, { id: "ab12cd34ef", status: "provisioning" }),
    json(200, { id: "ab12cd34ef", status: "stopping" }),
    json(201, { id: "b".repeat(32) }),
    json(200, { id: "ab12cd34ef", status: "updating" }),
  );
  const w = new Warren(API_URL, KEY, { fetch: r.fetchImpl });
  await w.createInstance({ template: "warren-agent", type: "vm", resources: { cpu: 2, disk: 8 }, auto_sleep: true, name: undefined });
  let call = r.last();
  assert.equal(call.method, "POST");
  assert.equal(call.url, `${API_URL}/v1/instances`);
  assert.equal(call.headers.get("content-type"), "application/json");
  assert.deepEqual(JSON.parse(call.body as string), { template: "warren-agent", type: "vm", resources: { cpu: 2, disk: 8 }, auto_sleep: true });

  await w.stopInstance("ab12cd34ef");
  call = r.last();
  assert.equal(call.url, `${API_URL}/v1/instances/ab12cd34ef/stop`);
  assert.equal(call.method, "POST");
  assert.equal(call.body, undefined);

  await w.createBackup("ab12cd34ef");
  assert.equal(r.last().body, "{}");

  await w.updateInstance("ab12cd34ef", { template: "my-agent@3" });
  assert.equal(r.last().body, JSON.stringify({ template: "my-agent@3" }));
});

test("query strings and path segments are encoded", async () => {
  const r = recorder(
    json(200, { logs: "hello" }),
    json(200, { logs: "" }),
    json(200, { url: "http://x", expires_at: 1 }),
    json(200, { data: [] }),
    json(200, { name: "my-agent", revision: 2 }),
    json(200, { id: "c d", deleted: true }),
  );
  const w = new Warren(API_URL, KEY, { fetch: r.fetchImpl });
  assert.equal(await w.logs("ab12cd34ef", { tail: 200, since: 1763000000 }), "hello");
  assert.equal(r.last().url, `${API_URL}/v1/instances/ab12cd34ef/logs?tail=200&since=1763000000`);

  await w.logs("ab12cd34ef");
  assert.equal(r.last().url, `${API_URL}/v1/instances/ab12cd34ef/logs`);

  await w.signedUrl("ab12cd34ef", { port: 7681, ttl: 3600, path: "/a b" });
  const u = new URL(r.last().url);
  assert.equal(u.pathname, "/v1/instances/ab12cd34ef/signed-url");
  assert.equal(u.searchParams.get("port"), "7681");
  assert.equal(u.searchParams.get("ttl"), "3600");
  assert.equal(u.searchParams.get("path"), "/a b");

  const ws = await w.workspace();
  assert.equal(r.last().url, `${API_URL}/v1/workspace`);
  assert.equal(ws.awake_limit, undefined); // the recorder answers {} by default

  await w.getTemplate("my-agent@2");
  assert.equal(r.last().url, `${API_URL}/v1/templates/my-agent@2`);

  await w.deleteCron("ab12cd34ef", "c d");
  assert.equal(r.last().url, `${API_URL}/v1/instances/ab12cd34ef/crons/c%20d`);
  assert.equal(r.last().method, "DELETE");

  const e = await rejects(w.getInstance(""));
  assert.equal(e.code, "invalid_request");
});

test("lists are unwrapped and signup works without a key", async () => {
  const r = recorder(
    json(200, { data: [{ id: "a" }, { id: "b" }] }),
    json(200, { data: [{ at: 1, cpu_percent: 0.5, memory_bytes: 1, disk_bytes: 2 }] }),
    json(201, { workspace: { id: "ws_x" }, api_key: "sk_live_x", key_id: "k" }),
  );
  const w = new Warren(API_URL, KEY, { fetch: r.fetchImpl });
  assert.deepEqual((await w.listInstances()).map((i) => i.id), ["a", "b"]);
  assert.equal((await w.metrics("ab12cd34ef", 2))[0]?.at, 1);
  assert.equal(r.last().url, `${API_URL}/v1/instances/ab12cd34ef/metrics?hours=2`);

  const anon = new Warren(API_URL, undefined, { fetch: r.fetchImpl });
  const signup = await anon.signup({ name: "me" });
  assert.equal(signup.api_key, "sk_live_x");
  assert.equal(r.last().headers.get("authorization"), null);
  assert.equal(r.last().body, JSON.stringify({ name: "me" }));
});

test("exec accepts a string or a request and extends the timeout", async () => {
  const r = recorder(json(200, { exit_code: 0, stdout: "hi\n", stderr: "", duration_ms: 3 }), json(200, { exit_code: 1, stdout: "", stderr: "", duration_ms: 1 }));
  const w = new Warren(API_URL, KEY, { fetch: r.fetchImpl });
  const out = await w.exec("ab12cd34ef", "echo hi");
  assert.equal(out.stdout, "hi\n");
  assert.equal(r.last().body, JSON.stringify({ command: "echo hi" }));
  await w.exec("ab12cd34ef", { command: "sleep 100", user: "root", timeout_seconds: 120, env: { A: "1" } });
  assert.deepEqual(JSON.parse(r.last().body as string), { command: "sleep 100", user: "root", timeout_seconds: 120, env: { A: "1" } });
  const e = await rejects(w.exec("ab12cd34ef", ""));
  assert.equal(e.code, "invalid_request");
});

test("non-2xx answers reject with the decoded WarrenError", async () => {
  const body = { error: { code: "instance_limit_reached", message: "This workspace may keep 3 instances awake at once and 3 are awake.", details: { limit: 3, awake: 3 } } };
  const r = recorder(json(409, body), text(502, "this template has no default port\n"));
  const w = new Warren(API_URL, KEY, { fetch: r.fetchImpl });
  const e = await rejects(w.createInstance());
  assert.equal(e.code, "instance_limit_reached");
  assert.equal(e.status, 409);
  assert.deepEqual(e.details, { limit: 3, awake: 3 });
  const g = await rejects(w.getInstance("ab12cd34ef"));
  assert.equal(g.code, "bad_gateway");
  assert.equal(g.status, 502);
});

test("transport failures become try_again, timeouts become timeout, user aborts pass through", async () => {
  const failing = recorder(new TypeError("fetch failed"));
  const w = new Warren(API_URL, KEY, { fetch: failing.fetchImpl });
  const e = await rejects(w.health());
  assert.equal(e.code, "try_again");
  assert.equal(e.status, 0);
  assert.match(e.message, /\/v1\/health/);

  const hanging = recorder(
    (call) =>
      new Promise<Response>((_, reject) => {
        call.signal?.addEventListener("abort", () => reject(call.signal?.reason), { once: true });
      }),
  );
  const slow = new Warren(API_URL, KEY, { fetch: hanging.fetchImpl, timeoutMs: 20 });
  const t = await rejects(slow.health());
  assert.equal(t.code, "timeout");

  const hanging2 = recorder(
    (call) =>
      new Promise<Response>((_, reject) => {
        call.signal?.addEventListener("abort", () => reject(call.signal?.reason), { once: true });
      }),
  );
  const controller = new AbortController();
  const w2 = new Warren(API_URL, KEY, { fetch: hanging2.fetchImpl, timeoutMs: 0 });
  const pending = w2.health({ signal: controller.signal });
  controller.abort(new Error("user abort"));
  await assert.rejects(pending, (err: unknown) => err instanceof Error && err.message === "user abort" && !(err instanceof WarrenError));
});

test("waitFor polls until the status is reached and reports failures", async () => {
  const r = recorder(
    json(200, { id: "ab12cd34ef", status: "provisioning" }),
    json(200, { id: "ab12cd34ef", status: "starting" }),
    json(200, { id: "ab12cd34ef", status: "running" }),
    json(200, { id: "ab12cd34ef", status: "failed", status_reason: "image pull failed" }),
  );
  const w = new Warren(API_URL, KEY, { fetch: r.fetchImpl });
  const inst = await w.waitFor("ab12cd34ef", "running", { pollMs: 1 });
  assert.equal(inst.status, "running");
  assert.equal(r.calls.length, 3);
  const e = await rejects(w.waitFor("ab12cd34ef", ["running", "sleeping"], { pollMs: 1 }));
  assert.equal(e.code, "conflict");
  assert.match(e.message, /image pull failed/);
});

test("instanceUrl derives from the API host and agent() reuses the key", () => {
  const w = new Warren(API_URL, KEY, { fetch: recorder().fetchImpl });
  assert.equal(w.instanceUrl("ab12cd34ef"), INSTANCE_URL);
  assert.equal(w.instanceUrl("ab12cd34ef", 7681), "http://ab12cd34ef-7681.warren.localhost:7337");
  assert.equal(new Warren("https://api.example.com", KEY, { fetch: recorder().fetchImpl }).instanceUrl("ab12cd34ef"), "https://ab12cd34ef.example.com");
  const agent = w.agent({ id: "ab12cd34ef", url: `${INSTANCE_URL}/` } as Parameters<Warren["agent"]>[0]);
  assert.ok(agent instanceof Agent);
  assert.equal(agent.instanceUrl, INSTANCE_URL);
  assert.equal(agent.apiKey, KEY);
  assert.equal(w.agent("http://other:3737").instanceUrl, "http://other:3737");
});

// ---------------------------------------------------------------------------
// Agent API
// ---------------------------------------------------------------------------

test("Agent sends x-warren-key and the responses body", async () => {
  const r = recorder(json(200, { id: "r1", session_id: "s1", status: "completed", output_text: "echo: hi", created_at: 1 }));
  const agent = new Agent(INSTANCE_URL, KEY, { fetch: r.fetchImpl });
  const resp = await agent.respond({ input: "hi", session_id: "s1", metadata: { k: 1 } });
  assert.equal(resp.output_text, "echo: hi");
  const call = r.last();
  assert.equal(call.url, `${INSTANCE_URL}/v1/responses`);
  assert.equal(call.headers.get("x-warren-key"), KEY);
  assert.equal(call.headers.get("authorization"), null);
  assert.deepEqual(JSON.parse(call.body as string), { input: "hi", session_id: "s1", metadata: { k: 1 } });
});

test("Agent.stream parses the event stream and sets the streaming headers", async () => {
  const r = recorder(sse([SAMPLE_STREAM.slice(0, 40), SAMPLE_STREAM.slice(40)]));
  const agent = new Agent(INSTANCE_URL, KEY, { fetch: r.fetchImpl });
  const events: StreamEvent[] = await collect(agent.stream({ input: "hi", session_id: "s1" }));
  assert.deepEqual(events, EXPECTED_EVENTS);
  const call = r.last();
  assert.equal(call.method, "POST");
  assert.equal(call.url, `${INSTANCE_URL}/v1/responses`);
  assert.equal(call.headers.get("accept"), "text/event-stream");
  assert.equal(call.headers.get("content-type"), "application/json");
  assert.equal(call.headers.get("x-warren-key"), KEY);
  assert.deepEqual(JSON.parse(call.body as string), { input: "hi", session_id: "s1", stream: true });

  let textOut = "";
  for (const ev of events) if (ev.event === "response.output_text.delta") textOut += ev.data.delta;
  assert.equal(textOut, "Hello");
});

test("Agent.streamResponse replays with GET; stream errors use the envelope", async () => {
  const r = recorder(
    sse(['event: response.completed\ndata: {"id":"r1"}\n\n']),
    json(404, { error: { code: "not_supported", message: "no agent" } }),
    json(200, { id: "r1" }),
  );
  const agent = new Agent(INSTANCE_URL, KEY, { fetch: r.fetchImpl });
  assert.deepEqual(await collect(agent.streamResponse("r1")), [{ event: "response.completed", data: { id: "r1" } }]);
  assert.equal(r.last().method, "GET");
  assert.equal(r.last().url, `${INSTANCE_URL}/v1/responses/r1/stream`);
  assert.equal(r.last().body, undefined);

  const e = await rejects(collect(agent.stream("hi")));
  assert.equal(e.code, "not_supported");
  assert.equal(e.status, 404);
  assert.equal(e.message, "no agent");

  const wrong = await rejects(collect(agent.stream("hi")));
  assert.equal(wrong.code, "internal");
  assert.match(wrong.message, /text\/event-stream/);
});

test("Agent files: list, read, write, delete, move, download", async () => {
  const r = recorder(
    json(200, { path: "notes", entries: [{ name: "a b.md", type: "file", size: 3, modified_at: 1 }] }),
    text(200, "hello"),
    json(200, { path: "notes/c.txt", size: 5 }),
    json(200, { path: "x.bin", size: 2 }),
    json(200, { path: "x.bin", deleted: true }),
    json(200, { path: "b" }),
    new Response(new Uint8Array([0x1f, 0x8b]), { status: 200, headers: { "content-type": "application/gzip" } }),
  );
  const agent = new Agent(INSTANCE_URL, KEY, { fetch: r.fetchImpl });

  const listing = await agent.listFiles("notes");
  assert.equal(listing.entries[0]?.name, "a b.md");
  assert.equal(r.last().url, `${INSTANCE_URL}/v1/files?path=notes`);

  assert.equal(await agent.readTextFile("notes/a b.md"), "hello");
  assert.equal(new URL(r.last().url).searchParams.get("path"), "notes/a b.md");
  assert.equal(r.last().headers.get("accept"), "*/*");

  await agent.writeFile("notes/c.txt", "hello");
  let call = r.last();
  assert.equal(call.method, "PUT");
  assert.equal(call.url, `${INSTANCE_URL}/v1/files/content?path=notes%2Fc.txt`);
  assert.equal(call.body, "hello");
  assert.equal(call.headers.get("content-type"), "text/plain; charset=utf-8");

  await agent.writeFile("x.bin", new Uint8Array([0, 1]));
  call = r.last();
  assert.equal(call.headers.get("content-type"), "application/octet-stream");
  assert.deepEqual([...(call.body as Uint8Array)], [0, 1]);

  await agent.deleteFile("x.bin");
  assert.equal(r.last().method, "DELETE");
  assert.equal(r.last().url, `${INSTANCE_URL}/v1/files?path=x.bin`);

  await agent.moveFile("a", "b");
  assert.equal(r.last().url, `${INSTANCE_URL}/v1/files/move`);
  assert.equal(r.last().body, JSON.stringify({ from: "a", to: "b" }));

  const tar = await agent.download("notes");
  assert.deepEqual([...tar], [0x1f, 0x8b]);
  assert.equal(r.last().url, `${INSTANCE_URL}/v1/files/download?path=notes`);
});

test("Agent sessions, models, cancel and exec with the instance token", async () => {
  const r = recorder(
    json(200, { data: [{ id: "s1", name: "", created_at: 1, updated_at: 1, message_count: 2 }] }),
    json(200, { id: "s1", name: "renamed", messages: [] }),
    json(200, { data: [{ id: "default" }] }),
    json(200, { id: "r1", status: "cancelled" }),
    json(200, { exit_code: 0, stdout: "", stderr: "", duration_ms: 1 }),
  );
  const agent = new Agent(INSTANCE_URL, KEY, { fetch: r.fetchImpl });
  assert.equal((await agent.listSessions())[0]?.id, "s1");
  await agent.renameSession("s1", "renamed");
  assert.equal(r.last().method, "PATCH");
  assert.equal(r.last().body, JSON.stringify({ name: "renamed" }));
  assert.deepEqual(await agent.models(), [{ id: "default" }]);
  await agent.cancel("r1");
  assert.equal(r.last().url, `${INSTANCE_URL}/v1/responses/r1/cancel`);
  await agent.exec("tok123", { command: "uname -a", workdir: "/home/agent" });
  const call = r.last();
  assert.equal(call.url, `${INSTANCE_URL}/v1/exec`);
  assert.equal(call.headers.get("x-warren-instance-token"), "tok123");
  assert.equal(call.headers.get("x-warren-key"), KEY);
  assert.deepEqual(JSON.parse(call.body as string), { command: "uname -a", workdir: "/home/agent" });
});
