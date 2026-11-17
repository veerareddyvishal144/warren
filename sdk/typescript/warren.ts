/**
 * Warren client for TypeScript and JavaScript: the Sandbox API and the Agent
 * API in one file, built on `fetch`. Runs in Node 20+ and in browsers with no
 * dependencies.
 *
 * ```ts
 * import { Warren } from "./warren.ts";
 *
 * const w = new Warren("http://api.warren.localhost:7337", "sk_live_...");
 * let inst = await w.createInstance({ template: "warren-agent", auto_sleep: true });
 * inst = await w.waitFor(inst.id, "running");
 *
 * const agent = w.agent(inst);
 * for await (const ev of agent.stream("List the files in my home directory")) {
 *   if (ev.event === "response.output_text.delta") process.stdout.write(ev.data.delta);
 * }
 * ```
 *
 * Every failed call rejects with {@link WarrenError} carrying the stable error
 * `code`, the server `message`, the HTTP `status` (0 when no answer arrived)
 * and the optional `details` object. Timestamps are unix seconds on the
 * Sandbox API and unix milliseconds on the Agent API, exactly as on the wire.
 *
 * The Sandbox API is authenticated with `Authorization: Bearer <key>`;
 * requests to an instance URL carry the same key in the `x-warren-key` header.
 */

export const VERSION = "0.1.0";

// ---------------------------------------------------------------------------
// Wire types: Sandbox API
// ---------------------------------------------------------------------------

export type InstanceStatus =
  | "provisioning"
  | "running"
  | "stopping"
  | "stopped"
  | "starting"
  | "restarting"
  | "updating"
  | "sleeping"
  | "waking"
  | "failed"
  | "deleting"
  | "deleted";

export type InstanceType = "default" | "vm" | "performance";
export type NetworkMode = "allow-all" | "deny-all";
export type TemplateScope = "system" | "workspace";

/** CPU count, memory in GB and disk in GB. */
export interface Shape {
  cpu: number;
  memory: number;
  disk: number;
}

/** Partial shape as accepted in requests. */
export interface ShapeRequest {
  cpu?: number;
  memory?: number;
  disk?: number;
}

/** A tenant: many instances, many API keys. */
export interface Workspace {
  id: string;
  name: string;
  created_at: number;
}

/** `GET /v1/workspace`: the workspace plus its awake-instance counters. */
export interface WorkspaceInfo extends Workspace {
  /** Instances the workspace may keep awake at once; null means unlimited. */
  awake_limit: number | null;
  awake_count: number;
}

/** An API key as listed; the plaintext is only shown when created. */
export interface ApiKey {
  id: string;
  workspace_id: string;
  name: string;
  prefix: string;
  created_at: number;
  expires_at?: number;
  last_used_at?: number;
}

/** A named image plus the port its main service listens on. */
export interface Template {
  name: string;
  scope: TemplateScope;
  workspace_id?: string;
  image_ref: string;
  image_digest?: string;
  default_port: number;
  mount_path: string;
  agent: boolean;
  description?: string;
  types?: InstanceType[];
  default_shape: Shape;
  env?: Record<string, string>;
  revision: number;
  created_at: number;
  updated_at: number;
}

/** The instance object of `docs/API.md`. */
export interface Instance {
  id: string;
  workspace_id: string;
  name?: string;
  template: string;
  template_name: string;
  template_revision: number;
  image_ref: string;
  type: InstanceType;
  status: InstanceStatus;
  status_reason?: string;
  resources: Shape;
  region: string;
  metadata?: Record<string, string>;
  env?: Record<string, string>;
  network: NetworkMode;
  auto_sleep: boolean;
  idle_timeout_seconds: number;
  url: string;
  default_port: number;
  mount_path: string;
  agent_api: boolean;
  parent_id?: string;
  created_at: number;
  updated_at: number;
  started_at?: number;
  stopped_at?: number;
  slept_at?: number;
  last_activity_at?: number;
  runtime: string;
  disk_used_bytes: number;
}

/** A restore point for an instance's disk. */
export interface Backup {
  id: string;
  instance_id: string;
  workspace_id: string;
  size_bytes: number;
  created_at: number;
  note?: string;
}

/** A scheduled prompt for an agent instance. */
export interface Cron {
  id: string;
  instance_id: string;
  workspace_id: string;
  name?: string;
  prompt: string;
  schedule: string;
  timezone: string;
  enabled: boolean;
  last_run_at?: number;
  last_status?: string;
  next_run_at?: number;
  created_at: number;
  updated_at: number;
}

/** One sample of `GET /v1/instances/{id}/metrics`. */
export interface MetricPoint {
  at: number;
  cpu_percent: number;
  memory_bytes: number;
  disk_bytes: number;
}

/** `GET /v1/health` of the daemon. */
export interface DaemonHealth {
  healthy: boolean;
  version: string;
  runtimes: string[];
  time: number;
}

export interface VersionInfo {
  version: string;
}

export interface SignupRequest {
  name?: string;
}

/** `POST /v1/signup`; `api_key` is shown once. */
export interface SignupResult {
  workspace: Workspace;
  api_key: string;
  key_id: string;
}

export interface CreateApiKeyRequest {
  name?: string;
  /** Unix seconds; omitted means never. */
  expires_at?: number;
}

/** `POST /v1/api-keys`; `api_key` is shown once. */
export interface CreateApiKeyResult {
  key: ApiKey;
  api_key: string;
}

/** Body of `POST /v1/templates`. */
export interface RegisterTemplateRequest {
  /** `[a-z0-9-]{3,40}`, not starting with `warren-`. */
  name: string;
  image_ref: string;
  default_port?: number;
  mount_path?: string;
  agent?: boolean;
  description?: string;
  env?: Record<string, string>;
}

/** Body of `POST /v1/instances`; every field is optional. */
export interface CreateInstanceRequest {
  template?: string;
  resources?: ShapeRequest;
  type?: InstanceType;
  region?: string;
  name?: string;
  metadata?: Record<string, string>;
  env?: Record<string, string>;
  auto_sleep?: boolean;
  idle_timeout_seconds?: number;
  network?: NetworkMode;
}

/** Body of `PATCH /v1/instances/{id}`; absent fields stay, maps are replaced whole. */
export interface PatchInstanceRequest {
  name?: string;
  metadata?: Record<string, string>;
  env?: Record<string, string>;
  auto_sleep?: boolean;
  idle_timeout_seconds?: number;
  network?: NetworkMode;
}

/** Body of `POST /v1/instances/{id}/fork`. */
export interface ForkRequest {
  name?: string;
  resources?: ShapeRequest;
  metadata?: Record<string, string>;
  auto_sleep?: boolean;
  idle_timeout_seconds?: number;
}

/** Body of `POST /v1/instances/{id}/resize`; disk may only grow. */
export type ResizeRequest = ShapeRequest;

/** Body of `POST /v1/instances/{id}/update`: `name` or `name@rev`. */
export interface UpdateInstanceRequest {
  template?: string;
}

/** One shell command inside an instance. */
export interface ExecRequest {
  command: string;
  /** Empty means the image user; `"root"` is allowed. */
  user?: string;
  timeout_seconds?: number;
  workdir?: string;
  env?: Record<string, string>;
}

export interface ExecResult {
  exit_code: number;
  stdout: string;
  stderr: string;
  duration_ms: number;
  /** Set when a stream was cut at 1 MiB. */
  truncated?: boolean;
}

export interface LogsOptions {
  tail?: number;
  /** Unix seconds. */
  since?: number;
}

export interface SignedUrlOptions {
  /** Omitted means the template's default port; 7681 is the terminal. */
  port?: number;
  /** 60 .. 86400 seconds. */
  ttl?: number;
  path?: string;
}

export interface SignedUrl {
  url: string;
  expires_at: number;
}

export interface CreateBackupRequest {
  note?: string;
}

export interface CreateCronRequest {
  prompt: string;
  /** Five-field cron expression. */
  schedule: string;
  /** IANA name; omitted means UTC. */
  timezone?: string;
  name?: string;
  enabled?: boolean;
}

export interface PatchCronRequest {
  prompt?: string;
  schedule?: string;
  timezone?: string;
  name?: string;
  enabled?: boolean;
}

/** Answer of stop/start/sleep/restart/delete. */
export interface StatusChange {
  id: string;
  status: InstanceStatus;
}

export interface ResizeResult extends StatusChange {
  resources: Shape;
}

export interface UpdateResult extends StatusChange {
  image_ref: string;
  template_revision: number;
}

export interface Deleted {
  id: string;
  deleted: boolean;
}

export interface TemplateDeleted {
  name: string;
  deleted: boolean;
}

// ---------------------------------------------------------------------------
// Wire types: Agent API
// ---------------------------------------------------------------------------

/** `GET /v1/health` of the gateway inside an instance. */
export interface AgentHealth {
  healthy: boolean;
  agent_ready: boolean;
  driver: string;
  version: string;
}

/** Body of `POST /v1/responses`; `stream` is set by {@link Agent.stream}. */
export interface ResponsesRequest {
  input: string;
  /** Omit for a fresh session; reuse the returned id to continue. */
  session_id?: string;
  model?: string;
  files?: string[];
  metadata?: Record<string, unknown>;
}

export type ResponseStatus = "in_progress" | "completed" | "failed" | "cancelled";

/** A turn of the agent. Timestamps are unix milliseconds. */
export interface ResponseObject {
  id: string;
  session_id: string;
  status: ResponseStatus;
  output_text: string;
  created_at: number;
  completed_at?: number;
  usage?: Record<string, unknown>;
  error?: string;
}

export interface CancelResult {
  id: string;
  status: ResponseStatus;
}

export interface SessionSummary {
  id: string;
  name: string;
  created_at: number;
  updated_at: number;
  message_count: number;
}

export interface Message {
  role: string;
  content: string;
  at: number;
}

export interface SessionDetail {
  id: string;
  name: string;
  created_at?: number;
  updated_at?: number;
  messages: Message[];
}

export interface ModelInfo {
  id: string;
}

export interface FileEntry {
  name: string;
  type: "file" | "dir";
  size: number;
  modified_at: number;
}

export interface DirListing {
  path: string;
  entries: FileEntry[];
}

export interface WriteResult {
  path: string;
  size: number;
}

export interface FileDeleted {
  path: string;
  deleted: boolean;
}

export interface MoveRequest {
  from: string;
  to: string;
}

/** `response.created` */
export interface ResponseCreated {
  id: string;
  session_id: string;
}

/** `response.output_text.delta` */
export interface OutputTextDelta {
  delta: string;
}

/** `response.tool_call` */
export interface ToolCall {
  name: string;
  arguments: unknown;
}

/** `response.completed` */
export interface ResponseCompleted {
  id: string;
  session_id: string;
  output_text: string;
  status: ResponseStatus;
}

/** `response.failed` */
export interface ResponseFailed {
  id: string;
  error: string;
}

/**
 * One event of a response stream. Events named here are the ones the
 * contract defines; an event this client does not know is passed through with
 * `data` as decoded JSON (or the raw string), so always branch on `event`.
 */
export type StreamEvent =
  | { event: "response.created"; data: ResponseCreated }
  | { event: "response.output_text.delta"; data: OutputTextDelta }
  | { event: "response.tool_call"; data: ToolCall }
  | { event: "response.completed"; data: ResponseCompleted }
  | { event: "response.failed"; data: ResponseFailed };

/** A raw server-sent event before its `data` is decoded. */
export interface SSEEvent {
  event: string;
  data: string;
  id?: string;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/**
 * Every failure of this client. `code` is one of the stable API codes
 * (`invalid_request`, `invalid_api_key`, `forbidden`,
 * `not_found`, `conflict`, `instance_limit_reached`, `not_supported`,
 * `try_again`, `timeout`, `bad_gateway`, `internal`); `status` is the HTTP
 * status, or 0 when the request never completed.
 */
export class WarrenError extends Error {
  readonly code: string;
  readonly status: number;
  readonly details?: unknown;

  constructor(code: string, message: string, status = 0, details?: unknown) {
    super(message);
    this.name = "WarrenError";
    this.code = code;
    this.status = status;
    this.details = details;
  }

  /**
   * Decode a non-2xx body: the `{"error": {...}}` envelope, a bare error
   * object, or anything else (synthesised from the status and the text).
   */
  static fromBody(status: number, body: string): WarrenError {
    let parsed: unknown;
    try {
      parsed = body ? JSON.parse(body) : undefined;
    } catch {
      parsed = undefined;
    }
    if (isRecord(parsed)) {
      const inner = isRecord(parsed.error) ? parsed.error : parsed;
      if (typeof inner.code === "string" && inner.code) {
        const message = typeof inner.message === "string" ? inner.message : `HTTP ${status}`;
        return new WarrenError(inner.code, message, status, inner.details);
      }
    }
    const text = snippet(body);
    return new WarrenError(codeForStatus(status), text ? `HTTP ${status}: ${text}` : `HTTP ${status}`, status);
  }

  override toString(): string {
    return this.status ? `${this.code} (HTTP ${this.status}): ${this.message}` : `${this.code}: ${this.message}`;
  }
}

/** The error code the API would use for a bare HTTP status. */
export function codeForStatus(status: number): string {
  switch (status) {
    case 400:
      return "invalid_request";
    case 401:
      return "invalid_api_key";
    case 403:
      return "forbidden";
    case 404:
      return "not_found";
    case 408:
    case 504:
      return "timeout";
    case 409:
      return "conflict";
    case 429:
    case 503:
      return "try_again";
    case 502:
      return "bad_gateway";
    default:
      return "internal";
  }
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function snippet(text: string, limit = 300): string {
  const t = text.trim();
  return t.length <= limit ? t : `${t.slice(0, limit)}...`;
}

// ---------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------

/**
 * Incremental `text/event-stream` parser (WHATWG EventSource algorithm):
 * `event:` and `data:` fields, multi-line data joined with `\n`, `:` comment
 * lines ignored, a blank line dispatches, `\n`, `\r\n` and `\r` all end a
 * line. Events without data are dropped; the name defaults to `message`.
 */
export class SSEParser {
  private buffer = "";
  private eventName = "";
  private data: string[] = [];
  private lastId: string | undefined;

  /** Feed decoded text; returns every event completed by it. */
  feed(chunk: string): SSEEvent[] {
    this.buffer += chunk;
    const out: SSEEvent[] = [];
    let start = 0;
    for (;;) {
      let end = -1;
      for (let i = start; i < this.buffer.length; i++) {
        const c = this.buffer.charCodeAt(i);
        if (c === 10 || c === 13) {
          end = i;
          break;
        }
      }
      if (end < 0) break;
      let next = end + 1;
      if (this.buffer.charCodeAt(end) === 13) {
        // A trailing "\r" may be the first half of "\r\n": wait for more input.
        if (end + 1 >= this.buffer.length) break;
        if (this.buffer.charCodeAt(end + 1) === 10) next = end + 2;
      }
      const ev = this.line(this.buffer.slice(start, end));
      if (ev) out.push(ev);
      start = next;
    }
    this.buffer = this.buffer.slice(start);
    return out;
  }

  private line(line: string): SSEEvent | undefined {
    if (line === "") return this.dispatch();
    if (line.startsWith(":")) return undefined;
    const colon = line.indexOf(":");
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    switch (field) {
      case "event":
        this.eventName = value;
        break;
      case "data":
        this.data.push(value);
        break;
      case "id":
        if (!value.includes("\u0000")) this.lastId = value;
        break;
      default:
        break; // retry: and unknown fields are irrelevant for a one-shot stream
    }
    return undefined;
  }

  private dispatch(): SSEEvent | undefined {
    if (this.data.length === 0) {
      this.eventName = "";
      return undefined;
    }
    const ev: SSEEvent = { event: this.eventName || "message", data: this.data.join("\n") };
    if (this.lastId !== undefined) ev.id = this.lastId;
    this.eventName = "";
    this.data = [];
    return ev;
  }
}

/**
 * Parse a `text/event-stream` body. Pending bytes never terminated by a blank
 * line are discarded at end of stream. Returning early from the consuming
 * loop cancels the underlying stream.
 */
export async function* parseEventStream(stream: ReadableStream<Uint8Array>): AsyncGenerator<SSEEvent, void, undefined> {
  const reader = stream.getReader();
  const decoder = new TextDecoder("utf-8");
  const parser = new SSEParser();
  let finished = false;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      for (const ev of parser.feed(decoder.decode(value, { stream: true }))) yield ev;
    }
    finished = true;
    for (const ev of parser.feed(decoder.decode())) yield ev;
  } finally {
    if (!finished) await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}

function decodeData(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

/** Options shared by {@link Warren} and {@link Agent}. */
export interface ClientOptions {
  /** `fetch` implementation; defaults to `globalThis.fetch`. */
  fetch?: typeof fetch;
  /**
   * Time limit in milliseconds for a whole call, streams included; 0 disables
   * it. Defaults: 60,000 for {@link Warren}, 600,000 for {@link Agent}.
   */
  timeoutMs?: number;
  /** Extra headers sent with every request. */
  headers?: Record<string, string>;
}

/** Per-call options. */
export interface RequestOptions {
  signal?: AbortSignal;
  /** Overrides the client's `timeoutMs` for this call. */
  timeoutMs?: number;
}

type Query = Record<string, string | number | boolean | undefined>;

interface SendInit extends RequestOptions {
  query?: Query;
  json?: unknown;
  body?: BodyInit;
  contentType?: string;
  headers?: Record<string, string>;
  accept?: string;
}

/** Trim a base URL: no trailing slash, no `/v1` suffix (paths add it). */
function normalizeBase(url: string, what: string): string {
  if (typeof url !== "string" || !url.trim()) {
    throw new WarrenError("invalid_request", `${what} must be a non-empty URL`);
  }
  let base = url.trim().replace(/\/+$/, "");
  if (base.endsWith("/v1")) base = base.slice(0, -3).replace(/\/+$/, "");
  if (!/^https?:\/\/[^/]+/i.test(base)) {
    throw new WarrenError("invalid_request", `${what} must start with http:// or https://, got ${JSON.stringify(url)}`);
  }
  return base;
}

function encodeQuery(query: Query | undefined): string {
  if (!query) return "";
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined) continue;
    params.append(key, String(value));
  }
  const text = params.toString();
  return text ? `?${text}` : "";
}

/** Percent-encode one path segment (keeps `@` so `name@rev` stays readable). */
function seg(value: string): string {
  if (typeof value !== "string" || !value) {
    throw new WarrenError("invalid_request", "identifier must be a non-empty string");
  }
  return encodeURIComponent(value).replace(/%40/g, "@");
}

/** Drop `undefined` values so absent fields are not sent (the API rejects unknown fields). */
function compact<T extends object>(body: T): Partial<T> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(body)) if (v !== undefined) out[k] = v;
  return out as Partial<T>;
}

function unwrapList<T>(payload: unknown): T[] {
  if (isRecord(payload) && Array.isArray(payload.data)) return payload.data as T[];
  if (Array.isArray(payload)) return payload as T[];
  throw new WarrenError("internal", `expected a list envelope, got ${typeof payload}`);
}

function hasUnref(timer: unknown): timer is { unref(): void } {
  return typeof timer === "object" && timer !== null && typeof (timer as { unref?: unknown }).unref === "function";
}

/** A signal that aborts on the caller's signal or after `timeoutMs`. */
function deadline(signal: AbortSignal | undefined, timeoutMs: number, url: string) {
  const controller = new AbortController();
  const onAbort = () => controller.abort(signal?.reason);
  if (signal) {
    if (signal.aborted) controller.abort(signal.reason);
    else signal.addEventListener("abort", onAbort, { once: true });
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  if (timeoutMs > 0) {
    timer = setTimeout(() => {
      controller.abort(new WarrenError("timeout", `request to ${url} timed out after ${timeoutMs} ms`));
    }, timeoutMs);
    if (hasUnref(timer)) timer.unref();
  }
  return {
    signal: controller.signal,
    cleanup: () => {
      if (timer !== undefined) clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
    },
  };
}

/** Map a thrown value from `fetch` or a body read to a {@link WarrenError} (user aborts pass through). */
function transportError(e: unknown, url: string, userSignal: AbortSignal | undefined, context: string): unknown {
  if (e instanceof WarrenError) return e;
  if (userSignal?.aborted) return e;
  const message = e instanceof Error ? e.message : String(e);
  const cause = e instanceof Error && e.cause instanceof Error ? `: ${e.cause.message}` : "";
  return new WarrenError("try_again", `${context} ${url} failed: ${message}${cause}`, 0);
}

class Transport {
  readonly baseUrl: string;
  private readonly headers: Record<string, string>;
  private readonly fetchImpl: typeof fetch;
  private readonly timeoutMs: number;

  constructor(baseUrl: string, what: string, auth: Record<string, string>, options: ClientOptions, defaultTimeoutMs: number) {
    this.baseUrl = normalizeBase(baseUrl, what);
    this.headers = { ...(options.headers ?? {}), ...auth };
    const impl = options.fetch ?? globalThis.fetch;
    if (typeof impl !== "function") {
      throw new WarrenError("invalid_request", "no fetch implementation: pass one in options.fetch");
    }
    this.fetchImpl = impl;
    this.timeoutMs = options.timeoutMs ?? defaultTimeoutMs;
  }

  url(path: string, query?: Query): string {
    return this.baseUrl + path + encodeQuery(query);
  }

  /** Send a request; resolves with the 2xx `Response`, rejects with {@link WarrenError} otherwise. */
  async send(method: string, path: string, init: SendInit = {}): Promise<{ res: Response; release: () => void }> {
    const url = this.url(path, init.query);
    const headers = new Headers(this.headers);
    headers.set("accept", init.accept ?? "application/json");
    for (const [k, v] of Object.entries(init.headers ?? {})) headers.set(k, v);
    let body: BodyInit | undefined;
    if (init.json !== undefined) {
      body = JSON.stringify(init.json);
      headers.set("content-type", "application/json");
    } else if (init.body !== undefined) {
      body = init.body;
      headers.set("content-type", init.contentType ?? "application/octet-stream");
    }
    const timeoutMs = init.timeoutMs ?? this.timeoutMs;
    const dl = deadline(init.signal, timeoutMs, url);
    let res: Response;
    try {
      res = await this.fetchImpl(url, { method, headers, body, signal: dl.signal });
    } catch (e) {
      dl.cleanup();
      throw transportError(e, url, init.signal, `${method}`);
    }
    if (!res.ok) {
      let text = "";
      try {
        text = await res.text();
      } catch {
        text = "";
      }
      dl.cleanup();
      throw WarrenError.fromBody(res.status, text);
    }
    return { res, release: dl.cleanup };
  }

  async json<T>(method: string, path: string, init: SendInit = {}): Promise<T> {
    const { res, release } = await this.send(method, path, init);
    try {
      const text = await res.text();
      if (!text.trim()) return undefined as T;
      try {
        return JSON.parse(text) as T;
      } catch (e) {
        throw new WarrenError("internal", `cannot decode response from ${this.url(path)}: ${(e as Error).message}`, res.status);
      }
    } catch (e) {
      throw transportError(e, this.url(path), init.signal, "reading the response from");
    } finally {
      release();
    }
  }

  async bytes(method: string, path: string, init: SendInit = {}): Promise<Uint8Array> {
    const { res, release } = await this.send(method, path, { accept: "*/*", ...init });
    try {
      return new Uint8Array(await res.arrayBuffer());
    } catch (e) {
      throw transportError(e, this.url(path), init.signal, "reading the response from");
    } finally {
      release();
    }
  }

  /** Open a `text/event-stream` response and yield its decoded events. */
  async *events(method: string, path: string, init: SendInit = {}): AsyncGenerator<StreamEvent, void, undefined> {
    const url = this.url(path, init.query);
    const { res, release } = await this.send(method, path, { ...init, accept: "text/event-stream" });
    try {
      const ctype = res.headers.get("content-type") ?? "";
      if (!ctype.toLowerCase().includes("text/event-stream")) {
        const text = await res.text().catch(() => "");
        throw new WarrenError(
          "internal",
          `expected text/event-stream from ${url}, got ${ctype || "no content type"}: ${snippet(text)}`,
          res.status,
        );
      }
      if (!res.body) throw new WarrenError("internal", `response from ${url} has no body`, res.status);
      for await (const ev of parseEventStream(res.body)) {
        yield { event: ev.event, data: decodeData(ev.data) } as StreamEvent;
      }
    } catch (e) {
      throw transportError(e, url, init.signal, "stream from");
    } finally {
      release();
    }
  }
}

function toBody(data: string | Uint8Array | ArrayBuffer | Blob): BodyInit {
  if (typeof data === "string" || data instanceof ArrayBuffer || data instanceof Blob) return data;
  // A Uint8Array over any buffer is a valid fetch body at runtime; the DOM
  // typings only accept views over a plain ArrayBuffer, so view it explicitly.
  return new Uint8Array(data.buffer, data.byteOffset, data.byteLength) as Uint8Array<ArrayBuffer>;
}

function execBody(req: ExecRequest | string): ExecRequest {
  const body = typeof req === "string" ? { command: req } : req;
  if (typeof body.command !== "string" || !body.command) {
    throw new WarrenError("invalid_request", "command must be a non-empty string");
  }
  return compact(body) as ExecRequest;
}

/** Give an exec call at least the command's own timeout plus slack. */
function execTimeout(req: ExecRequest, options: RequestOptions | undefined, clientDefault: number): number | undefined {
  if (options?.timeoutMs !== undefined) return options.timeoutMs;
  if (clientDefault === 0 || !req.timeout_seconds) return undefined;
  const needed = (req.timeout_seconds + 30) * 1000;
  return needed > clientDefault ? needed : undefined;
}

// ---------------------------------------------------------------------------
// Sandbox API
// ---------------------------------------------------------------------------

/** Options for {@link Warren.waitFor}. */
export interface WaitOptions extends RequestOptions {
  /** Give up after this long; default 180,000 ms. */
  timeoutMs?: number;
  /** Poll interval; default 1,000 ms. */
  pollMs?: number;
}

/**
 * Client for the Sandbox API (`/v1` on `api.<base>`).
 *
 * List endpoints return plain arrays (the `{"data": [...]}` envelope is
 * unwrapped); everything else resolves with the JSON object.
 */
export class Warren {
  readonly apiUrl: string;
  readonly apiKey: string | undefined;
  private readonly http: Transport;
  private readonly options: ClientOptions;

  /**
   * @param apiUrl `http://api.warren.localhost:7337`, with or without `/v1`.
   * @param apiKey `sk_live_...`; only {@link signup}, {@link health} and
   *   {@link version} work without one.
   */
  constructor(apiUrl: string, apiKey?: string, options: ClientOptions = {}) {
    this.apiKey = apiKey?.trim() || undefined;
    this.options = options;
    const auth: Record<string, string> = this.apiKey ? { authorization: `Bearer ${this.apiKey}` } : {};
    this.http = new Transport(apiUrl, "apiUrl", auth, options, 60_000);
    this.apiUrl = this.http.baseUrl;
  }

  // -- health, version, signup ----------------------------------------------

  /** `GET /v1/health`. No auth. */
  async health(options?: RequestOptions): Promise<DaemonHealth> {
    return this.http.json("GET", "/v1/health", options);
  }

  /** `GET /v1/version`. No auth. */
  async version(options?: RequestOptions): Promise<string> {
    return (await this.http.json<VersionInfo>("GET", "/v1/version", options)).version;
  }

  /** `POST /v1/signup`: a new workspace and its first key, shown once. No auth. */
  async signup(req: SignupRequest = {}, options?: RequestOptions): Promise<SignupResult> {
    return this.http.json("POST", "/v1/signup", { ...options, json: compact(req) });
  }

  // -- workspace, keys ------------------------------------------------------

  /** `GET /v1/workspace`. */
  async workspace(options?: RequestOptions): Promise<WorkspaceInfo> {
    return this.http.json("GET", "/v1/workspace", options);
  }

  /** `GET /v1/api-keys`. */
  async listApiKeys(options?: RequestOptions): Promise<ApiKey[]> {
    return unwrapList<ApiKey>(await this.http.json("GET", "/v1/api-keys", options));
  }

  /** `POST /v1/api-keys`; the plaintext key is shown once. */
  async createApiKey(req: CreateApiKeyRequest = {}, options?: RequestOptions): Promise<CreateApiKeyResult> {
    return this.http.json("POST", "/v1/api-keys", { ...options, json: compact(req) });
  }

  /** `DELETE /v1/api-keys/{id}`. */
  async deleteApiKey(keyId: string, options?: RequestOptions): Promise<Deleted> {
    return this.http.json("DELETE", `/v1/api-keys/${seg(keyId)}`, options);
  }

  // -- templates ------------------------------------------------------------

  /** `GET /v1/templates`: system catalog first, then workspace templates. */
  async listTemplates(options?: RequestOptions): Promise<Template[]> {
    return unwrapList<Template>(await this.http.json("GET", "/v1/templates", options));
  }

  /** `GET /v1/templates/{name}`; `name@<revision>` pins a workspace revision. */
  async getTemplate(name: string, options?: RequestOptions): Promise<Template> {
    return this.http.json("GET", `/v1/templates/${seg(name)}`, options);
  }

  /** `POST /v1/templates`: revision 1, or a new revision when the name exists. */
  async registerTemplate(req: RegisterTemplateRequest, options?: RequestOptions): Promise<Template> {
    return this.http.json("POST", "/v1/templates", { ...options, json: compact(req) });
  }

  /** `DELETE /v1/templates/{name}`; `conflict` while instances use it. */
  async deleteTemplate(name: string, options?: RequestOptions): Promise<TemplateDeleted> {
    return this.http.json("DELETE", `/v1/templates/${seg(name)}`, options);
  }

  // -- instances ------------------------------------------------------------

  /** `POST /v1/instances`: the new instance in status `provisioning` (see {@link waitFor}). */
  async createInstance(req: CreateInstanceRequest = {}, options?: RequestOptions): Promise<Instance> {
    return this.http.json("POST", "/v1/instances", { ...options, json: compact(req) });
  }

  /** `GET /v1/instances`, newest first. */
  async listInstances(options?: RequestOptions): Promise<Instance[]> {
    return unwrapList<Instance>(await this.http.json("GET", "/v1/instances", options));
  }

  /** `GET /v1/instances/{id}`. */
  async getInstance(id: string, options?: RequestOptions): Promise<Instance> {
    return this.http.json("GET", inst(id), options);
  }

  /** `PATCH /v1/instances/{id}`; env changes apply at the next start. */
  async patchInstance(id: string, req: PatchInstanceRequest, options?: RequestOptions): Promise<Instance> {
    return this.http.json("PATCH", inst(id), { ...options, json: compact(req) });
  }

  /** `DELETE /v1/instances/{id}`: destroys the instance and settles the final minutes. */
  async deleteInstance(id: string, options?: RequestOptions): Promise<StatusChange> {
    return this.http.json("DELETE", inst(id), options);
  }

  /** `POST /v1/instances/{id}/stop`: frees CPU and memory; never wakes on request. */
  async stopInstance(id: string, options?: RequestOptions): Promise<StatusChange> {
    return this.http.json("POST", inst(id, "stop"), options);
  }

  /** `POST /v1/instances/{id}/start` from stopped or sleeping. */
  async startInstance(id: string, options?: RequestOptions): Promise<StatusChange> {
    return this.http.json("POST", inst(id, "start"), options);
  }

  /** `POST /v1/instances/{id}/sleep`; any request to the instance URL wakes it. */
  async sleepInstance(id: string, options?: RequestOptions): Promise<StatusChange> {
    return this.http.json("POST", inst(id, "sleep"), options);
  }

  /** `POST /v1/instances/{id}/restart`. */
  async restartInstance(id: string, options?: RequestOptions): Promise<StatusChange> {
    return this.http.json("POST", inst(id, "restart"), options);
  }

  /** `POST /v1/instances/{id}/resize`; disk may only grow. */
  async resizeInstance(id: string, req: ResizeRequest, options?: RequestOptions): Promise<ResizeResult> {
    return this.http.json("POST", inst(id, "resize"), { ...options, json: compact(req) });
  }

  /** `POST /v1/instances/{id}/update`: replaces the image, keeps the data. */
  async updateInstance(id: string, req: UpdateInstanceRequest = {}, options?: RequestOptions): Promise<UpdateResult> {
    return this.http.json("POST", inst(id, "update"), { ...options, json: compact(req) });
  }

  /** `POST /v1/instances/{id}/fork`: a copy of the disk with a new id and URL. */
  async forkInstance(id: string, req: ForkRequest = {}, options?: RequestOptions): Promise<Instance> {
    return this.http.json("POST", inst(id, "fork"), { ...options, json: compact(req) });
  }

  /** `POST /v1/instances/{id}/exec`: runs a shell command; wakes a sleeping instance. */
  async exec(id: string, req: ExecRequest | string, options?: RequestOptions): Promise<ExecResult> {
    const body = execBody(req);
    const timeoutMs = execTimeout(body, options, this.options.timeoutMs ?? 60_000);
    return this.http.json("POST", inst(id, "exec"), { ...options, ...(timeoutMs !== undefined ? { timeoutMs } : {}), json: body });
  }

  /** `GET /v1/instances/{id}/logs?tail=&since=`. */
  async logs(id: string, opts: LogsOptions = {}, options?: RequestOptions): Promise<string> {
    const r = await this.http.json<{ logs: string }>("GET", inst(id, "logs"), { ...options, query: { tail: opts.tail, since: opts.since } });
    return r.logs;
  }

  /** `GET /v1/instances/{id}/metrics?hours=`: one point per minute while running. */
  async metrics(id: string, hours?: number, options?: RequestOptions): Promise<MetricPoint[]> {
    return unwrapList<MetricPoint>(await this.http.json("GET", inst(id, "metrics"), { ...options, query: { hours } }));
  }

  /** `GET /v1/instances/{id}/signed-url?port=&ttl=&path=`: a URL that opens without the key. */
  async signedUrl(id: string, opts: SignedUrlOptions = {}, options?: RequestOptions): Promise<SignedUrl> {
    return this.http.json("GET", inst(id, "signed-url"), { ...options, query: { port: opts.port, ttl: opts.ttl, path: opts.path } });
  }

  // -- backups --------------------------------------------------------------

  /** `GET /v1/instances/{id}/backups`. */
  async listBackups(id: string, options?: RequestOptions): Promise<Backup[]> {
    return unwrapList<Backup>(await this.http.json("GET", inst(id, "backups"), options));
  }

  /** `POST /v1/instances/{id}/backups`, taken without stopping. */
  async createBackup(id: string, req: CreateBackupRequest = {}, options?: RequestOptions): Promise<Backup> {
    return this.http.json("POST", inst(id, "backups"), { ...options, json: compact(req) });
  }

  /** `POST /v1/instances/{id}/restore`: stopped, restored, restarted if it was running. */
  async restore(id: string, backupId: string, options?: RequestOptions): Promise<Instance> {
    return this.http.json("POST", inst(id, "restore"), { ...options, json: { backup_id: backupId } });
  }

  // -- crons ----------------------------------------------------------------

  /** `POST /v1/instances/{id}/crons`; agent templates only. */
  async createCron(id: string, req: CreateCronRequest, options?: RequestOptions): Promise<Cron> {
    return this.http.json("POST", inst(id, "crons"), { ...options, json: compact(req) });
  }

  /** `GET /v1/instances/{id}/crons`. */
  async listCrons(id: string, options?: RequestOptions): Promise<Cron[]> {
    return unwrapList<Cron>(await this.http.json("GET", inst(id, "crons"), options));
  }

  /** `GET /v1/instances/{id}/crons/{cid}`. */
  async getCron(id: string, cronId: string, options?: RequestOptions): Promise<Cron> {
    return this.http.json("GET", inst(id, "crons", cronId), options);
  }

  /** `PATCH /v1/instances/{id}/crons/{cid}`. */
  async patchCron(id: string, cronId: string, req: PatchCronRequest, options?: RequestOptions): Promise<Cron> {
    return this.http.json("PATCH", inst(id, "crons", cronId), { ...options, json: compact(req) });
  }

  /** `DELETE /v1/instances/{id}/crons/{cid}`. */
  async deleteCron(id: string, cronId: string, options?: RequestOptions): Promise<Deleted> {
    return this.http.json("DELETE", inst(id, "crons", cronId), options);
  }

  // -- conveniences ---------------------------------------------------------

  /**
   * Poll `GET /v1/instances/{id}` until one of `status` is reached. Rejects
   * with `conflict` when the instance becomes `failed` or `deleted` while
   * waiting for something else, and with `timeout` after `timeoutMs`.
   */
  async waitFor(id: string, status: InstanceStatus | InstanceStatus[] = "running", opts: WaitOptions = {}): Promise<Instance> {
    const wanted = new Set(Array.isArray(status) ? status : [status]);
    const timeoutMs = opts.timeoutMs ?? 180_000;
    const pollMs = opts.pollMs ?? 1_000;
    const started = Date.now();
    const perCall = { signal: opts.signal, timeoutMs: this.options.timeoutMs };
    for (;;) {
      const current = await this.getInstance(id, perCall);
      if (wanted.has(current.status)) return current;
      if (current.status === "failed" || current.status === "deleted") {
        const reason = current.status_reason ? `: ${current.status_reason}` : "";
        throw new WarrenError("conflict", `instance ${id} is ${current.status}${reason}`, 0, {
          status: current.status,
          status_reason: current.status_reason ?? "",
        });
      }
      if (Date.now() - started >= timeoutMs) {
        throw new WarrenError(
          "timeout",
          `instance ${id} still ${current.status} after ${timeoutMs} ms (wanted ${[...wanted].join(", ")})`,
          0,
          { status: current.status },
        );
      }
      await sleep(pollMs, opts.signal);
    }
  }

  /**
   * Derive `http://<id>.<base>[:port]` (or `<id>-<port>.<base>`) from the API
   * URL. `instance.url` from the API is authoritative; this is for preview
   * URLs of other ports.
   */
  instanceUrl(id: string, port?: number): string {
    const u = new URL(this.apiUrl);
    const host = u.hostname.startsWith("api.") ? u.hostname.slice(4) : u.hostname;
    const label = port ? `${id}-${port}` : id;
    return `${u.protocol}//${label}.${host}${u.port ? `:${u.port}` : ""}`;
  }

  /**
   * An {@link Agent} for an instance (its `url`) or an instance URL, using this
   * client's key and options.
   */
  agent(instance: Instance | string, options: ClientOptions = {}): Agent {
    const url = typeof instance === "string" ? instance : instance.url;
    if (!url) throw new WarrenError("invalid_request", "instance has no url");
    const { timeoutMs: _ignored, ...inherited } = this.options;
    return new Agent(url, this.apiKey, { ...inherited, ...options });
  }
}

function inst(id: string, ...rest: string[]): string {
  let path = `/v1/instances/${seg(id)}`;
  for (const part of rest) path += `/${seg(part)}`;
  return path;
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    function onAbort() {
      clearTimeout(timer);
      reject(signal?.reason);
    }
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

// ---------------------------------------------------------------------------
// Agent API
// ---------------------------------------------------------------------------

/**
 * Client for the Agent API served inside one instance.
 *
 * Reach it through the instance URL (`instance.url`, authenticated and woken
 * by the edge) or directly at `http://<ip>:3737` from inside the network.
 * File paths are relative to the persistent home directory.
 */
export class Agent {
  readonly instanceUrl: string;
  readonly apiKey: string | undefined;
  private readonly http: Transport;
  private readonly options: ClientOptions;

  /**
   * @param instanceUrl `http://<id>.<base>[:port]`.
   * @param apiKey workspace key, sent as `x-warren-key`; optional when the
   *   gateway is reached directly without the edge.
   */
  constructor(instanceUrl: string, apiKey?: string, options: ClientOptions = {}) {
    this.apiKey = apiKey?.trim() || undefined;
    this.options = options;
    const auth: Record<string, string> = this.apiKey ? { "x-warren-key": this.apiKey } : {};
    this.http = new Transport(instanceUrl, "instanceUrl", auth, options, 600_000);
    this.instanceUrl = this.http.baseUrl;
  }

  // -- health ---------------------------------------------------------------

  /** `GET /v1/health`. */
  async health(options?: RequestOptions): Promise<AgentHealth> {
    return this.http.json("GET", "/v1/health", options);
  }

  /** `GET /v1/version`. */
  async version(options?: RequestOptions): Promise<string> {
    return (await this.http.json<VersionInfo>("GET", "/v1/version", options)).version;
  }

  // -- responses ------------------------------------------------------------

  /**
   * `POST /v1/responses` without streaming: resolves when the turn is
   * `completed` or `failed`. A string is shorthand for `{ input }`.
   * Rejects with `not_supported` (404) on a template without an agent.
   */
  async respond(req: ResponsesRequest | string, options?: RequestOptions): Promise<ResponseObject> {
    return this.http.json("POST", "/v1/responses", { ...options, json: responsesBody(req, undefined) });
  }

  /**
   * `POST /v1/responses` with `stream: true`: yields {@link StreamEvent}s
   * (`response.created`, `response.output_text.delta`, `response.tool_call`,
   * `response.completed`, `response.failed`). Returning early from the loop
   * closes the connection.
   */
  async *stream(req: ResponsesRequest | string, options?: RequestOptions): AsyncGenerator<StreamEvent, void, undefined> {
    yield* this.http.events("POST", "/v1/responses", { ...options, json: responsesBody(req, true) });
  }

  /** `GET /v1/responses/{id}`. */
  async getResponse(responseId: string, options?: RequestOptions): Promise<ResponseObject> {
    return this.http.json("GET", `/v1/responses/${seg(responseId)}`, options);
  }

  /** `GET /v1/responses/{id}/stream`: replays stored events, then follows a live turn. */
  async *streamResponse(responseId: string, options?: RequestOptions): AsyncGenerator<StreamEvent, void, undefined> {
    yield* this.http.events("GET", `/v1/responses/${seg(responseId)}/stream`, options);
  }

  /** `POST /v1/responses/{id}/cancel`. */
  async cancel(responseId: string, options?: RequestOptions): Promise<CancelResult> {
    return this.http.json("POST", `/v1/responses/${seg(responseId)}/cancel`, options);
  }

  // -- sessions and models --------------------------------------------------

  /** `GET /v1/sessions`, newest first. */
  async listSessions(options?: RequestOptions): Promise<SessionSummary[]> {
    return unwrapList<SessionSummary>(await this.http.json("GET", "/v1/sessions", options));
  }

  /** `GET /v1/sessions/{id}`: the transcript. */
  async getSession(sessionId: string, options?: RequestOptions): Promise<SessionDetail> {
    return this.http.json("GET", `/v1/sessions/${seg(sessionId)}`, options);
  }

  /** `PATCH /v1/sessions/{id}` with `{ name }`. */
  async renameSession(sessionId: string, name: string, options?: RequestOptions): Promise<SessionDetail> {
    return this.http.json("PATCH", `/v1/sessions/${seg(sessionId)}`, { ...options, json: { name } });
  }

  /** `DELETE /v1/sessions/{id}`. */
  async deleteSession(sessionId: string, options?: RequestOptions): Promise<Deleted> {
    return this.http.json("DELETE", `/v1/sessions/${seg(sessionId)}`, options);
  }

  /** `GET /v1/models`. */
  async models(options?: RequestOptions): Promise<ModelInfo[]> {
    return unwrapList<ModelInfo>(await this.http.json("GET", "/v1/models", options));
  }

  // -- files ----------------------------------------------------------------

  /** `GET /v1/files?path=`. */
  async listFiles(path = ".", options?: RequestOptions): Promise<DirListing> {
    return this.http.json("GET", "/v1/files", { ...options, query: { path } });
  }

  /** `GET /v1/files/content?path=`: the raw bytes. */
  async readFile(path: string, options?: RequestOptions): Promise<Uint8Array> {
    return this.http.bytes("GET", "/v1/files/content", { ...options, query: { path } });
  }

  /** {@link readFile} decoded as UTF-8 text. */
  async readTextFile(path: string, options?: RequestOptions): Promise<string> {
    return new TextDecoder("utf-8").decode(await this.readFile(path, options));
  }

  /** `PUT /v1/files/content?path=` with the raw body; creates parent directories. */
  async writeFile(path: string, data: string | Uint8Array | ArrayBuffer | Blob, contentType?: string, options?: RequestOptions): Promise<WriteResult> {
    const type = contentType ?? (typeof data === "string" ? "text/plain; charset=utf-8" : data instanceof Blob && data.type ? data.type : "application/octet-stream");
    return this.http.json("PUT", "/v1/files/content", { ...options, query: { path }, body: toBody(data), contentType: type });
  }

  /** `DELETE /v1/files?path=`. */
  async deleteFile(path: string, options?: RequestOptions): Promise<FileDeleted> {
    return this.http.json("DELETE", "/v1/files", { ...options, query: { path } });
  }

  /** `POST /v1/files/move` with `{ from, to }`. */
  async moveFile(from: string, to: string, options?: RequestOptions): Promise<{ path: string }> {
    return this.http.json("POST", "/v1/files/move", { ...options, json: { from, to } satisfies MoveRequest });
  }

  /** `GET /v1/files/download?path=`: a gzip tarball of the directory. */
  async download(path = ".", options?: RequestOptions): Promise<Uint8Array> {
    return this.http.bytes("GET", "/v1/files/download", { ...options, query: { path } });
  }

  // -- exec -----------------------------------------------------------------

  /**
   * `POST /v1/exec` with `x-warren-instance-token: <token>`. The token is the
   * per-instance secret only the daemon knows (`$WARREN_INSTANCE_TOKEN`
   * inside the instance); customers normally use {@link Warren.exec}.
   */
  async exec(token: string, req: ExecRequest | string, options?: RequestOptions): Promise<ExecResult> {
    const body = execBody(req);
    const timeoutMs = execTimeout(body, options, this.options.timeoutMs ?? 600_000);
    return this.http.json("POST", "/v1/exec", {
      ...options,
      ...(timeoutMs !== undefined ? { timeoutMs } : {}),
      json: body,
      headers: { "x-warren-instance-token": token },
    });
  }
}

function responsesBody(req: ResponsesRequest | string, stream: true | undefined): Record<string, unknown> {
  const body = typeof req === "string" ? { input: req } : req;
  if (typeof body.input !== "string") throw new WarrenError("invalid_request", "input must be a string");
  return compact({ ...body, stream });
}
