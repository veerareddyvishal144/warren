"""Warren client for Python: the Sandbox API and the Agent API in one file.

Standard library only: ``urllib`` for ordinary requests and ``http.client``
for streaming server-sent events. Python 3.8 or newer.

Quick start::

    from warren import Warren, WarrenError

    w = Warren("http://api.warren.localhost:7337", "sk_live_...")
    inst = w.create_instance(template="warren-agent", auto_sleep=True)
    inst = w.wait_for(inst["id"], "running")

    agent = w.agent(inst)                      # Agent client for the instance URL
    for event, data in agent.stream("List the files in my home directory"):
        if event == "response.output_text.delta":
            print(data["delta"], end="", flush=True)

Every failed call raises :class:`WarrenError` carrying the stable error ``code``
(``not_found``, ``conflict``, ``instance_limit_reached``, ...), the server's
``message``, the HTTP ``status`` (0 when the request never completed) and the
optional ``details`` object. Timestamps are unix seconds on the Sandbox API,
unix milliseconds on the Agent API, exactly as on the wire.

The Sandbox API is authenticated with ``Authorization: Bearer <key>``; requests
to an instance URL carry the same key in the ``x-warren-key`` header.
"""

from __future__ import annotations

import http.client
import json
import os
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Dict, Iterable, Iterator, List, Optional, Tuple, Union

try:  # Python 3.8+
    from typing import TypedDict
except ImportError:  # pragma: no cover - very old interpreters
    TypedDict = dict  # type: ignore[assignment,misc]

__all__ = [
    "Agent",
    "Warren",
    "WarrenError",
    "parse_sse",
    "EVENT_CREATED",
    "EVENT_DELTA",
    "EVENT_TOOL_CALL",
    "EVENT_COMPLETED",
    "EVENT_FAILED",
]

__version__ = "0.1.0"

#: Named events on a response stream (``event:`` field of the SSE frames).
EVENT_CREATED = "response.created"
EVENT_DELTA = "response.output_text.delta"
EVENT_TOOL_CALL = "response.tool_call"
EVENT_COMPLETED = "response.completed"
EVENT_FAILED = "response.failed"

#: Header carrying the workspace key on instance URLs.
HEADER_API_KEY = "x-warren-key"
#: Header carrying the per-instance secret on the gateway's ``/v1/exec``.
HEADER_INSTANCE_TOKEN = "x-warren-instance-token"

_DEFAULT_API_URL = "http://api.warren.localhost:7337"
_USER_AGENT = "warren-python/" + __version__
_STREAM_CHUNK = 65536

# ---------------------------------------------------------------------------
# Wire types (documentation-grade hints; the client returns plain dicts)
# ---------------------------------------------------------------------------

JSON = Dict[str, Any]


class Shape(TypedDict, total=False):
    """CPU count, memory in GB and disk in GB."""

    cpu: int
    memory: int
    disk: int


class Workspace(TypedDict, total=False):
    """A tenant: many instances, many API keys. ``GET /v1/workspace`` adds
    ``awake_limit`` (``None`` means unlimited) and ``awake_count``."""

    id: str
    name: str
    created_at: int
    awake_limit: Optional[int]
    awake_count: int


class ApiKey(TypedDict, total=False):
    """An API key as listed by ``GET /v1/api-keys`` (never the plaintext)."""

    id: str
    workspace_id: str
    name: str
    prefix: str
    created_at: int
    expires_at: int
    last_used_at: int


class Template(TypedDict, total=False):
    """A named image plus the port its main service listens on."""

    name: str
    scope: str
    workspace_id: str
    image_ref: str
    image_digest: str
    default_port: int
    mount_path: str
    agent: bool
    description: str
    types: List[str]
    default_shape: Shape
    env: Dict[str, str]
    revision: int
    created_at: int
    updated_at: int


class Instance(TypedDict, total=False):
    """The instance object of ``docs/API.md``."""

    id: str
    workspace_id: str
    name: str
    template: str
    template_name: str
    template_revision: int
    image_ref: str
    type: str
    status: str
    status_reason: str
    resources: Shape
    region: str
    metadata: Dict[str, str]
    env: Dict[str, str]
    network: str
    auto_sleep: bool
    idle_timeout_seconds: int
    url: str
    default_port: int
    mount_path: str
    agent_api: bool
    parent_id: str
    created_at: int
    updated_at: int
    started_at: int
    stopped_at: int
    slept_at: int
    last_activity_at: int
    runtime: str
    disk_used_bytes: int


class Backup(TypedDict, total=False):
    """A restore point for an instance's disk."""

    id: str
    instance_id: str
    workspace_id: str
    size_bytes: int
    created_at: int
    note: str


class Cron(TypedDict, total=False):
    """A scheduled prompt for an agent instance."""

    id: str
    instance_id: str
    workspace_id: str
    name: str
    prompt: str
    schedule: str
    timezone: str
    enabled: bool
    last_run_at: int
    last_status: str
    next_run_at: int
    created_at: int
    updated_at: int


class MetricPoint(TypedDict, total=False):
    """One sample of ``GET /v1/instances/{id}/metrics``."""

    at: int
    cpu_percent: float
    memory_bytes: int
    disk_bytes: int


class ExecResult(TypedDict, total=False):
    """Outcome of one shell command."""

    exit_code: int
    stdout: str
    stderr: str
    duration_ms: int
    truncated: bool


class ResponseObject(TypedDict, total=False):
    """A turn of the agent (``POST /v1/responses`` without streaming)."""

    id: str
    session_id: str
    status: str
    output_text: str
    created_at: int
    completed_at: int
    usage: JSON
    error: str


class SessionSummary(TypedDict, total=False):
    """One row of ``GET /v1/sessions``."""

    id: str
    name: str
    created_at: int
    updated_at: int
    message_count: int


class Message(TypedDict, total=False):
    """One message of a session transcript."""

    role: str
    content: str
    at: int


class SessionDetail(TypedDict, total=False):
    """``GET /v1/sessions/{id}``."""

    id: str
    name: str
    created_at: int
    updated_at: int
    messages: List[Message]


class FileEntry(TypedDict, total=False):
    """One entry of a directory listing."""

    name: str
    type: str
    size: int
    modified_at: int


class DirListing(TypedDict, total=False):
    """``GET /v1/files?path=``."""

    path: str
    entries: List[FileEntry]


#: One parsed server-sent event: ``(event name, data)``. ``data`` is the decoded
#: JSON payload when the ``data:`` field holds JSON, otherwise the raw string.
StreamEvent = Tuple[str, Any]


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


class WarrenError(Exception):
    """Raised for every failed call.

    :param code: stable error code (``invalid_request``, ``invalid_api_key``,
        ``forbidden``, ``not_found``, ``conflict``,
        ``instance_limit_reached``, ``not_supported``, ``try_again``,
        ``timeout``, ``bad_gateway``, ``internal``).
    :param message: human-readable explanation from the server or the client.
    :param status: HTTP status of the answer; 0 when no answer was received
        (connection failure, timeout, client-side validation).
    :param details: the optional ``details`` object of the error envelope.
    """

    def __init__(self, code: str, message: str, status: int = 0, details: Optional[Any] = None):
        super().__init__(message)
        self.code = code
        self.message = message
        self.status = status
        self.details = details

    def __str__(self) -> str:
        if self.status:
            return "%s (HTTP %d): %s" % (self.code, self.status, self.message)
        return "%s: %s" % (self.code, self.message)

    def __repr__(self) -> str:
        return "WarrenError(code=%r, message=%r, status=%r, details=%r)" % (
            self.code,
            self.message,
            self.status,
            self.details,
        )


def _code_for_status(status: int) -> str:
    """The error code the API would use for a bare HTTP status."""
    return {
        400: "invalid_request",
        401: "invalid_api_key",
        403: "forbidden",
        404: "not_found",
        408: "timeout",
        409: "conflict",
        429: "try_again",
        502: "bad_gateway",
        503: "try_again",
        504: "timeout",
    }.get(status, "internal")


def _snippet(body: bytes, limit: int = 300) -> str:
    text = body.decode("utf-8", "replace").strip()
    return text if len(text) <= limit else text[:limit] + "..."


def _error_from_body(status: int, body: bytes) -> WarrenError:
    """Decode a non-2xx body: the ``{"error": {...}}`` envelope, a bare error
    object, or anything else (synthesised from the status and the text)."""
    try:
        parsed = json.loads(body.decode("utf-8")) if body else None
    except ValueError:
        parsed = None
    if isinstance(parsed, dict):
        err = parsed.get("error")
        obj = err if isinstance(err, dict) else parsed
        code = obj.get("code")
        if isinstance(code, str) and code:
            message = obj.get("message")
            return WarrenError(
                code,
                message if isinstance(message, str) else "HTTP %d" % status,
                status,
                obj.get("details"),
            )
    text = _snippet(body)
    message = "HTTP %d: %s" % (status, text) if text else "HTTP %d" % status
    return WarrenError(_code_for_status(status), message, status)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _normalize_base(url: str, what: str) -> str:
    """Trim a base URL: no trailing slash, no ``/v1`` suffix (paths add it)."""
    if not isinstance(url, str) or not url.strip():
        raise WarrenError("invalid_request", "%s must be a non-empty URL" % what)
    base = url.strip().rstrip("/")
    if base.endswith("/v1"):
        base = base[: -len("/v1")].rstrip("/")
    parts = urllib.parse.urlsplit(base)
    if parts.scheme not in ("http", "https") or not parts.netloc:
        raise WarrenError("invalid_request", "%s must start with http:// or https://, got %r" % (what, url))
    return base


def _compact(fields: Dict[str, Any]) -> JSON:
    """Drop ``None`` values so absent fields are not sent (the API rejects
    unknown fields and treats absent ones as unchanged or default)."""
    return {k: v for k, v in fields.items() if v is not None}


def _encode_query(query: Optional[Dict[str, Any]]) -> str:
    if not query:
        return ""
    items: List[Tuple[str, str]] = []
    for key, value in query.items():
        if value is None:
            continue
        if isinstance(value, bool):
            value = "true" if value else "false"
        items.append((key, str(value)))
    if not items:
        return ""
    # ``quote`` keeps ``/`` readable in file paths; spaces become %20.
    return "?" + urllib.parse.urlencode(items, safe="/", quote_via=urllib.parse.quote)


def _unwrap_list(payload: Any) -> List[Any]:
    """Every list endpoint answers ``{"data": [...]}``."""
    if isinstance(payload, dict) and isinstance(payload.get("data"), list):
        return payload["data"]
    if isinstance(payload, list):
        return payload
    raise WarrenError("internal", "expected a list envelope, got %s" % type(payload).__name__)


def _decode_data(raw: str) -> Any:
    try:
        return json.loads(raw)
    except ValueError:
        return raw


def parse_sse(chunks: Iterable[bytes]) -> Iterator[StreamEvent]:
    """Parse ``text/event-stream`` bytes into ``(event, data)`` tuples.

    Follows the WHATWG EventSource algorithm: ``event:`` and ``data:`` fields,
    multi-line data joined with newlines, ``:`` comment lines ignored, a blank
    line dispatches, ``\\n``, ``\\r\\n`` and ``\\r`` all end a line. An event
    without ``data`` is dropped; the name defaults to ``message``. ``data`` is
    JSON-decoded when possible, otherwise the raw string. Pending bytes at end
    of stream that were never terminated by a blank line are discarded.
    """
    buffer = b""
    event_name = ""
    data_lines: List[str] = []

    def dispatch() -> Optional[StreamEvent]:
        nonlocal event_name, data_lines
        if not data_lines:
            event_name = ""
            return None
        out = (event_name or "message", _decode_data("\n".join(data_lines)))
        event_name = ""
        data_lines = []
        return out

    for chunk in chunks:
        if not chunk:
            continue
        buffer += chunk
        start = 0
        while True:
            nl = buffer.find(b"\n", start)
            cr = buffer.find(b"\r", start)
            if nl < 0 and cr < 0:
                break
            if cr >= 0 and (nl < 0 or cr < nl):
                if cr + 1 >= len(buffer):
                    break  # a lone trailing \r may be half of \r\n: wait for more
                end = cr
                nxt = cr + 2 if buffer[cr + 1 : cr + 2] == b"\n" else cr + 1
            else:
                end = nl
                nxt = nl + 1
            line = buffer[start:end]
            start = nxt
            if not line:
                ev = dispatch()
                if ev is not None:
                    yield ev
                continue
            if line.startswith(b":"):
                continue
            field, sep, value = line.partition(b":")
            if sep and value.startswith(b" "):
                value = value[1:]
            if field == b"event":
                event_name = value.decode("utf-8", "replace")
            elif field == b"data":
                data_lines.append(value.decode("utf-8", "replace"))
            # id: and retry: are irrelevant for a one-shot response stream
        buffer = buffer[start:]


# ---------------------------------------------------------------------------
# HTTP transport
# ---------------------------------------------------------------------------


class _Http:
    """Shared transport: ``urllib`` for one-shot calls, ``http.client`` for SSE."""

    def __init__(self, base_url: str, what: str, auth_headers: Dict[str, str], timeout: Optional[float]):
        self.base_url = _normalize_base(base_url, what)
        self.timeout = timeout
        self._headers: Dict[str, str] = {"Accept": "application/json", "User-Agent": _USER_AGENT}
        self._headers.update(auth_headers)

    def _url(self, path: str, query: Optional[Dict[str, Any]] = None) -> str:
        return self.base_url + path + _encode_query(query)

    def _request(
        self,
        method: str,
        path: str,
        query: Optional[Dict[str, Any]] = None,
        json_body: Optional[Any] = None,
        data: Optional[Union[bytes, str]] = None,
        content_type: Optional[str] = None,
        headers: Optional[Dict[str, str]] = None,
        raw: bool = False,
        timeout: Optional[float] = None,
    ) -> Any:
        """Send one request and return the decoded JSON (or bytes when ``raw``)."""
        url = self._url(path, query)
        hdrs = dict(self._headers)
        body: Optional[bytes] = None
        if json_body is not None:
            body = json.dumps(json_body).encode("utf-8")
            hdrs["Content-Type"] = "application/json"
        elif data is not None:
            body = data.encode("utf-8") if isinstance(data, str) else bytes(data)
            hdrs["Content-Type"] = content_type or "application/octet-stream"
        if headers:
            hdrs.update(headers)
        if raw:
            hdrs["Accept"] = "*/*"
        req = urllib.request.Request(url, data=body, method=method, headers=hdrs)
        effective_timeout = self.timeout if timeout is None else timeout
        try:
            with urllib.request.urlopen(req, timeout=effective_timeout) as resp:
                status = getattr(resp, "status", 200)
                payload = resp.read()
        except urllib.error.HTTPError as e:
            try:
                err_body = e.read()
            except Exception:  # noqa: BLE001 - the body is optional for diagnostics
                err_body = b""
            raise _error_from_body(e.code, err_body or b"") from None
        except urllib.error.URLError as e:
            reason = e.reason
            if isinstance(reason, (socket.timeout, TimeoutError)):
                raise WarrenError("timeout", "request to %s timed out" % url) from None
            raise WarrenError("try_again", "request to %s failed: %s" % (url, reason)) from None
        except (socket.timeout, TimeoutError):
            raise WarrenError("timeout", "request to %s timed out" % url) from None
        except (http.client.HTTPException, OSError) as e:
            raise WarrenError("try_again", "request to %s failed: %s" % (url, e)) from None
        if raw:
            return payload
        if not payload.strip():
            return None
        try:
            return json.loads(payload.decode("utf-8"))
        except ValueError as e:
            raise WarrenError("internal", "cannot decode response from %s: %s" % (url, e), status) from None

    def _stream(
        self,
        method: str,
        path: str,
        json_body: Optional[Any] = None,
        query: Optional[Dict[str, Any]] = None,
        timeout: Optional[float] = None,
    ) -> Iterator[StreamEvent]:
        """Open a ``text/event-stream`` response and yield its events.

        The connection is closed when the generator is exhausted, closed or
        garbage-collected. ``timeout`` is the socket timeout for the connect and
        for every read; ``None`` waits indefinitely.
        """
        url = self._url(path, query)
        parts = urllib.parse.urlsplit(url)
        conn_cls = http.client.HTTPSConnection if parts.scheme == "https" else http.client.HTTPConnection
        conn = conn_cls(parts.hostname or "", parts.port, timeout=timeout)
        hdrs = dict(self._headers)
        hdrs["Accept"] = "text/event-stream"
        body: Optional[bytes] = None
        if json_body is not None:
            body = json.dumps(json_body).encode("utf-8")
            hdrs["Content-Type"] = "application/json"
        target = parts.path or "/"
        if parts.query:
            target += "?" + parts.query
        try:
            conn.request(method, target, body=body, headers=hdrs)
            resp = conn.getresponse()
        except (socket.timeout, TimeoutError):
            conn.close()
            raise WarrenError("timeout", "request to %s timed out" % url) from None
        except (http.client.HTTPException, OSError) as e:
            conn.close()
            raise WarrenError("try_again", "request to %s failed: %s" % (url, e)) from None
        if resp.status >= 400:
            payload = resp.read()
            conn.close()
            raise _error_from_body(resp.status, payload)
        ctype = resp.getheader("Content-Type", "") or ""
        if "text/event-stream" not in ctype.lower():
            payload = resp.read()
            conn.close()
            raise WarrenError(
                "internal",
                "expected text/event-stream from %s, got %s: %s" % (url, ctype or "no content type", _snippet(payload)),
                resp.status,
            )
        return self._iter_events(conn, resp, url)

    @staticmethod
    def _iter_events(conn: http.client.HTTPConnection, resp: http.client.HTTPResponse, url: str) -> Iterator[StreamEvent]:
        def chunks() -> Iterator[bytes]:
            while True:
                chunk = resp.read1(_STREAM_CHUNK)
                if not chunk:
                    return
                yield chunk

        try:
            for event in parse_sse(chunks()):
                yield event
        except (socket.timeout, TimeoutError):
            raise WarrenError("timeout", "stream from %s timed out" % url) from None
        except (http.client.HTTPException, OSError) as e:
            raise WarrenError("try_again", "stream from %s interrupted: %s" % (url, e)) from None
        finally:
            conn.close()


# ---------------------------------------------------------------------------
# Sandbox API
# ---------------------------------------------------------------------------


class Warren(_Http):
    """Client for the Sandbox API (``/v1`` on ``api.<base>``).

    :param api_url: ``http://api.warren.localhost:7337`` (with or without
        ``/v1``). Defaults to ``$WARREN_API_URL`` or the local daemon.
    :param api_key: ``sk_live_...``; defaults to ``$WARREN_API_KEY``. Optional
        only for :meth:`signup`, :meth:`health` and :meth:`version`.
    :param timeout: socket timeout in seconds for every call (``None`` waits
        indefinitely). ``exec`` extends it to cover the command's own timeout.

    List endpoints return plain lists (the ``{"data": [...]}`` envelope is
    unwrapped); everything else returns the JSON object as a ``dict``.
    """

    def __init__(self, api_url: Optional[str] = None, api_key: Optional[str] = None, timeout: Optional[float] = 60.0):
        api_url = api_url or os.environ.get("WARREN_API_URL") or _DEFAULT_API_URL
        api_key = api_key if api_key is not None else os.environ.get("WARREN_API_KEY")
        self.api_key = api_key.strip() if api_key else None
        auth = {"Authorization": "Bearer " + self.api_key} if self.api_key else {}
        super().__init__(api_url, "api_url", auth, timeout)

    # -- health, version, signup ------------------------------------------

    def health(self) -> JSON:
        """``GET /v1/health`` -> ``{"healthy", "version", "runtimes", "time"}``. No auth."""
        return self._request("GET", "/v1/health")

    def version(self) -> str:
        """``GET /v1/version`` -> the daemon version string. No auth."""
        return self._request("GET", "/v1/version")["version"]

    def signup(self, name: Optional[str] = None) -> JSON:
        """``POST /v1/signup`` -> ``{"workspace", "api_key", "key_id"}``.

        Creates a workspace; the plaintext key is shown once. No auth required.
        """
        return self._request("POST", "/v1/signup", json_body=_compact({"name": name}))

    # -- workspace, keys --------------------------------------------------

    def workspace(self) -> Workspace:
        """``GET /v1/workspace`` -> workspace plus ``awake_limit`` (``None`` = unlimited) and ``awake_count``."""
        return self._request("GET", "/v1/workspace")

    def list_api_keys(self) -> List[ApiKey]:
        """``GET /v1/api-keys`` -> keys of the workspace (prefixes, never hashes)."""
        return _unwrap_list(self._request("GET", "/v1/api-keys"))

    def create_api_key(self, name: Optional[str] = None, expires_at: Optional[int] = None) -> JSON:
        """``POST /v1/api-keys`` -> ``{"key": {...}, "api_key": "sk_live_..."}``."""
        return self._request("POST", "/v1/api-keys", json_body=_compact({"name": name, "expires_at": expires_at}))

    def delete_api_key(self, key_id: str) -> JSON:
        """``DELETE /v1/api-keys/{id}`` -> ``{"id", "deleted": true}``."""
        return self._request("DELETE", "/v1/api-keys/" + _seg(key_id))

    # -- templates --------------------------------------------------------

    def list_templates(self) -> List[Template]:
        """``GET /v1/templates`` -> system catalog first, then workspace templates."""
        return _unwrap_list(self._request("GET", "/v1/templates"))

    def get_template(self, name: str) -> Template:
        """``GET /v1/templates/{name}``; ``name@<revision>`` pins a workspace revision."""
        return self._request("GET", "/v1/templates/" + _seg(name))

    def register_template(
        self,
        name: str,
        image_ref: str,
        default_port: int = 0,
        mount_path: str = "",
        agent: bool = False,
        description: str = "",
        env: Optional[Dict[str, str]] = None,
    ) -> Template:
        """``POST /v1/templates`` -> the template (revision 1, or a new revision
        when the name exists). ``name`` is ``[a-z0-9-]{3,40}`` and must not start
        with ``warren-``; the image is pulled and its digest recorded."""
        body: JSON = {
            "name": name,
            "image_ref": image_ref,
            "default_port": default_port,
            "mount_path": mount_path,
            "agent": agent,
            "description": description,
        }
        if env:
            body["env"] = env
        return self._request("POST", "/v1/templates", json_body=body)

    def delete_template(self, name: str) -> JSON:
        """``DELETE /v1/templates/{name}`` -> ``{"name", "deleted": true}``; ``conflict`` while instances use it."""
        return self._request("DELETE", "/v1/templates/" + _seg(name))

    # -- instances --------------------------------------------------------

    def create_instance(
        self,
        template: Optional[str] = None,
        resources: Optional[Shape] = None,
        type: Optional[str] = None,  # noqa: A002 - mirrors the wire field
        region: Optional[str] = None,
        name: Optional[str] = None,
        metadata: Optional[Dict[str, str]] = None,
        env: Optional[Dict[str, str]] = None,
        auto_sleep: Optional[bool] = None,
        idle_timeout_seconds: Optional[int] = None,
        network: Optional[str] = None,
    ) -> Instance:
        """``POST /v1/instances`` -> the new instance in status ``provisioning``.

        Every field is optional: template ``warren-base``, type ``default``,
        ``auto_sleep=False``, ``idle_timeout_seconds=900``, ``network="allow-all"``.
        ``resources`` is ``{"cpu", "memory", "disk"}`` (each optional). Use
        :meth:`wait_for` to block until it is ``running``.
        """
        body = _compact(
            {
                "template": template,
                "resources": resources,
                "type": type,
                "region": region,
                "name": name,
                "metadata": metadata,
                "env": env,
                "auto_sleep": auto_sleep,
                "idle_timeout_seconds": idle_timeout_seconds,
                "network": network,
            }
        )
        return self._request("POST", "/v1/instances", json_body=body)

    def list_instances(self) -> List[Instance]:
        """``GET /v1/instances`` -> instances of the workspace, newest first."""
        return _unwrap_list(self._request("GET", "/v1/instances"))

    def get_instance(self, instance_id: str) -> Instance:
        """``GET /v1/instances/{id}``."""
        return self._request("GET", _inst(instance_id))

    def patch_instance(
        self,
        instance_id: str,
        name: Optional[str] = None,
        metadata: Optional[Dict[str, str]] = None,
        env: Optional[Dict[str, str]] = None,
        auto_sleep: Optional[bool] = None,
        idle_timeout_seconds: Optional[int] = None,
        network: Optional[str] = None,
    ) -> Instance:
        """``PATCH /v1/instances/{id}`` -> the instance. Absent fields stay;
        ``metadata`` and ``env`` replace the whole map; env applies at next start."""
        body = _compact(
            {
                "name": name,
                "metadata": metadata,
                "env": env,
                "auto_sleep": auto_sleep,
                "idle_timeout_seconds": idle_timeout_seconds,
                "network": network,
            }
        )
        return self._request("PATCH", _inst(instance_id), json_body=body)

    def delete_instance(self, instance_id: str) -> JSON:
        """``DELETE /v1/instances/{id}`` -> ``{"id", "status": "deleting"}``."""
        return self._request("DELETE", _inst(instance_id))

    def stop_instance(self, instance_id: str) -> JSON:
        """``POST /v1/instances/{id}/stop`` -> ``{"id", "status": "stopping"}``.
        A stopped instance bills disk only and never wakes on request."""
        return self._request("POST", _inst(instance_id, "stop"))

    def start_instance(self, instance_id: str) -> JSON:
        """``POST /v1/instances/{id}/start`` -> ``{"id", "status": "starting"}``."""
        return self._request("POST", _inst(instance_id, "start"))

    def sleep_instance(self, instance_id: str) -> JSON:
        """``POST /v1/instances/{id}/sleep`` -> ``{"id", "status": "sleeping"}``.
        Any request to the instance URL wakes it again."""
        return self._request("POST", _inst(instance_id, "sleep"))

    def restart_instance(self, instance_id: str) -> JSON:
        """``POST /v1/instances/{id}/restart`` -> ``{"id", "status": "restarting"}``."""
        return self._request("POST", _inst(instance_id, "restart"))

    def resize_instance(
        self, instance_id: str, cpu: Optional[int] = None, memory: Optional[int] = None, disk: Optional[int] = None
    ) -> JSON:
        """``POST /v1/instances/{id}/resize`` -> ``{"id", "status", "resources"}``. Disk may only grow."""
        body = _compact({"cpu": cpu, "memory": memory, "disk": disk})
        return self._request("POST", _inst(instance_id, "resize"), json_body=body)

    def update_instance(self, instance_id: str, template: Optional[str] = None) -> JSON:
        """``POST /v1/instances/{id}/update`` -> ``{"id", "status": "updating", "image_ref", "template_revision"}``.
        ``template`` is ``name`` or ``name@rev``; omitted means the latest revision of the current template."""
        return self._request("POST", _inst(instance_id, "update"), json_body=_compact({"template": template}))

    def fork_instance(
        self,
        instance_id: str,
        name: Optional[str] = None,
        resources: Optional[Shape] = None,
        metadata: Optional[Dict[str, str]] = None,
        auto_sleep: Optional[bool] = None,
        idle_timeout_seconds: Optional[int] = None,
    ) -> Instance:
        """``POST /v1/instances/{id}/fork`` -> the new instance (copy of the disk,
        same template and env). A running source is slept during the copy."""
        body = _compact(
            {
                "name": name,
                "resources": resources,
                "metadata": metadata,
                "auto_sleep": auto_sleep,
                "idle_timeout_seconds": idle_timeout_seconds,
            }
        )
        return self._request("POST", _inst(instance_id, "fork"), json_body=body)

    def exec(
        self,
        instance_id: str,
        command: str,
        user: str = "",
        timeout_seconds: int = 0,
        workdir: str = "",
        env: Optional[Dict[str, str]] = None,
    ) -> ExecResult:
        """``POST /v1/instances/{id}/exec`` -> ``{"exit_code", "stdout", "stderr", "duration_ms", "truncated"}``.

        Runs ``command`` through the shell as the image user (or ``user="root"``).
        Wakes a sleeping instance; ``conflict`` when stopped. Output is capped at
        1 MiB per stream.
        """
        body = _exec_body(command, user, timeout_seconds, workdir, env)
        return self._request("POST", _inst(instance_id, "exec"), json_body=body, timeout=_exec_timeout(self.timeout, timeout_seconds))

    def logs(self, instance_id: str, tail: Optional[int] = None, since: Optional[int] = None) -> str:
        """``GET /v1/instances/{id}/logs?tail=&since=`` -> the log text."""
        return self._request("GET", _inst(instance_id, "logs"), query={"tail": tail, "since": since})["logs"]

    def metrics(self, instance_id: str, hours: Optional[int] = None) -> List[MetricPoint]:
        """``GET /v1/instances/{id}/metrics?hours=`` -> one point per minute while running."""
        return _unwrap_list(self._request("GET", _inst(instance_id, "metrics"), query={"hours": hours}))

    def signed_url(
        self, instance_id: str, port: Optional[int] = None, ttl: Optional[int] = None, path: Optional[str] = None
    ) -> JSON:
        """``GET /v1/instances/{id}/signed-url?port=&ttl=&path=`` -> ``{"url", "expires_at"}``.

        The URL opens the instance (or ``port``, e.g. 7681 for the terminal) in
        a browser without the API key. TTL 60..86400 seconds.
        """
        return self._request("GET", _inst(instance_id, "signed-url"), query={"port": port, "ttl": ttl, "path": path})

    # -- backups ----------------------------------------------------------

    def list_backups(self, instance_id: str) -> List[Backup]:
        """``GET /v1/instances/{id}/backups``."""
        return _unwrap_list(self._request("GET", _inst(instance_id, "backups")))

    def create_backup(self, instance_id: str, note: Optional[str] = None) -> Backup:
        """``POST /v1/instances/{id}/backups`` -> the backup, taken without stopping."""
        return self._request("POST", _inst(instance_id, "backups"), json_body=_compact({"note": note}))

    def restore(self, instance_id: str, backup_id: str) -> Instance:
        """``POST /v1/instances/{id}/restore`` -> the instance (stopped, restored,
        restarted if it was running)."""
        return self._request("POST", _inst(instance_id, "restore"), json_body={"backup_id": backup_id})

    # -- crons ------------------------------------------------------------

    def create_cron(
        self,
        instance_id: str,
        prompt: str,
        schedule: str,
        timezone: Optional[str] = None,
        name: Optional[str] = None,
        enabled: Optional[bool] = None,
    ) -> Cron:
        """``POST /v1/instances/{id}/crons`` -> the cron. Agent templates only;
        each firing wakes the instance and posts ``prompt`` to ``/v1/responses``."""
        body = _compact({"prompt": prompt, "schedule": schedule, "timezone": timezone, "name": name, "enabled": enabled})
        return self._request("POST", _inst(instance_id, "crons"), json_body=body)

    def list_crons(self, instance_id: str) -> List[Cron]:
        """``GET /v1/instances/{id}/crons``."""
        return _unwrap_list(self._request("GET", _inst(instance_id, "crons")))

    def get_cron(self, instance_id: str, cron_id: str) -> Cron:
        """``GET /v1/instances/{id}/crons/{cid}``."""
        return self._request("GET", _inst(instance_id, "crons", cron_id))

    def patch_cron(
        self,
        instance_id: str,
        cron_id: str,
        prompt: Optional[str] = None,
        schedule: Optional[str] = None,
        timezone: Optional[str] = None,
        name: Optional[str] = None,
        enabled: Optional[bool] = None,
    ) -> Cron:
        """``PATCH /v1/instances/{id}/crons/{cid}`` -> the cron; absent fields stay."""
        body = _compact({"prompt": prompt, "schedule": schedule, "timezone": timezone, "name": name, "enabled": enabled})
        return self._request("PATCH", _inst(instance_id, "crons", cron_id), json_body=body)

    def delete_cron(self, instance_id: str, cron_id: str) -> JSON:
        """``DELETE /v1/instances/{id}/crons/{cid}``."""
        return self._request("DELETE", _inst(instance_id, "crons", cron_id))

    # -- conveniences -----------------------------------------------------

    def wait_for(
        self,
        instance_id: str,
        status: Union[str, Iterable[str]] = "running",
        timeout: float = 180.0,
        poll: float = 1.0,
    ) -> Instance:
        """Poll ``GET /v1/instances/{id}`` until ``status`` (one or several) is
        reached and return the instance.

        Raises ``WarrenError("conflict")`` when the instance becomes ``failed`` or
        ``deleted`` while waiting for something else, and
        ``WarrenError("timeout")`` after ``timeout`` seconds.
        """
        wanted = {status} if isinstance(status, str) else set(status)
        deadline = time.monotonic() + timeout
        while True:
            inst = self.get_instance(instance_id)
            current = inst.get("status")
            if current in wanted:
                return inst
            if current in ("failed", "deleted"):
                reason = inst.get("status_reason") or ""
                raise WarrenError(
                    "conflict",
                    "instance %s is %s%s" % (instance_id, current, (": " + reason) if reason else ""),
                    0,
                    {"status": current, "status_reason": reason},
                )
            if time.monotonic() >= deadline:
                raise WarrenError(
                    "timeout",
                    "instance %s still %s after %.0fs (wanted %s)" % (instance_id, current, timeout, ", ".join(sorted(wanted))),
                    0,
                    {"status": current},
                )
            time.sleep(poll)

    def instance_url(self, instance_id: str, port: Optional[int] = None) -> str:
        """Derive ``http://<id>.<base>[:port]`` (or ``<id>-<port>.<base>``) from
        the API URL. ``instance["url"]`` as returned by the API is authoritative;
        this helper is for building preview URLs for other ports."""
        parts = urllib.parse.urlsplit(self.base_url)
        host = parts.hostname or ""
        base = host[4:] if host.startswith("api.") else host
        label = instance_id if not port else "%s-%d" % (instance_id, port)
        netloc = "%s.%s" % (label, base)
        if parts.port:
            netloc += ":%d" % parts.port
        return "%s://%s" % (parts.scheme, netloc)

    def agent(self, instance: Union[Instance, str], timeout: Optional[float] = 600.0) -> "Agent":
        """An :class:`Agent` for an instance, using this client's key.

        ``instance`` is the instance object (its ``url`` is used), an instance
        URL, or an instance id (looked up with :meth:`get_instance`).
        """
        if isinstance(instance, dict):
            url = instance.get("url") or ""
            if not url:
                raise WarrenError("invalid_request", "instance object has no url")
        elif isinstance(instance, str) and instance.startswith(("http://", "https://")):
            url = instance
        elif isinstance(instance, str) and instance:
            url = self.get_instance(instance).get("url") or self.instance_url(instance)
        else:
            raise WarrenError("invalid_request", "instance must be an instance object, URL or id")
        return Agent(url, self.api_key, timeout=timeout)


# ---------------------------------------------------------------------------
# Agent API
# ---------------------------------------------------------------------------


class Agent(_Http):
    """Client for the Agent API served inside one instance.

    :param instance_url: ``http://<id>.<base>[:port]`` as in ``instance["url"]``
        (the edge authenticates and wakes the instance), or
        ``http://<ip>:3737`` when talking to the gateway directly.
    :param api_key: workspace key, sent as ``x-warren-key``. Optional when the
        gateway is reached directly without the edge.
    :param timeout: socket timeout in seconds for one-shot calls; agent turns
        can be long, so the default is ten minutes. Streams take their own
        ``timeout`` argument.
    """

    def __init__(self, instance_url: str, api_key: Optional[str] = None, timeout: Optional[float] = 600.0):
        self.api_key = api_key.strip() if api_key else None
        auth = {HEADER_API_KEY: self.api_key} if self.api_key else {}
        super().__init__(instance_url, "instance_url", auth, timeout)

    # -- health -----------------------------------------------------------

    def health(self) -> JSON:
        """``GET /v1/health`` -> ``{"healthy", "agent_ready", "driver", "version"}``."""
        return self._request("GET", "/v1/health")

    def version(self) -> str:
        """``GET /v1/version`` -> the gateway version string."""
        return self._request("GET", "/v1/version")["version"]

    # -- responses --------------------------------------------------------

    def respond(
        self,
        input: str,  # noqa: A002 - mirrors the wire field
        session_id: Optional[str] = None,
        model: Optional[str] = None,
        files: Optional[List[str]] = None,
        metadata: Optional[JSON] = None,
    ) -> ResponseObject:
        """``POST /v1/responses`` without streaming -> the response object
        (``status`` ``completed`` or ``failed``, ``output_text``, ``usage``...).

        Omit ``session_id`` for a fresh session; reuse the returned
        ``session_id`` to continue a conversation. ``not_supported`` (404) on a
        template without an agent.
        """
        body = _responses_body(input, session_id, model, files, metadata, stream=None)
        return self._request("POST", "/v1/responses", json_body=body)

    def stream(
        self,
        input: str,  # noqa: A002 - mirrors the wire field
        session_id: Optional[str] = None,
        model: Optional[str] = None,
        files: Optional[List[str]] = None,
        metadata: Optional[JSON] = None,
        timeout: Optional[float] = None,
    ) -> Iterator[StreamEvent]:
        """``POST /v1/responses`` with ``stream=true`` -> generator of ``(event, data)``.

        Events: ``response.created`` ``{id, session_id}``,
        ``response.output_text.delta`` ``{delta}``, ``response.tool_call``
        ``{name, arguments}``, ``response.completed`` ``{id, session_id,
        output_text, status}``, ``response.failed`` ``{id, error}``. ``data`` is
        the decoded JSON object. ``timeout`` is a per-read socket timeout;
        ``None`` (default) waits as long as the agent takes.
        """
        body = _responses_body(input, session_id, model, files, metadata, stream=True)
        return self._stream("POST", "/v1/responses", json_body=body, timeout=timeout)

    def get_response(self, response_id: str) -> ResponseObject:
        """``GET /v1/responses/{id}``."""
        return self._request("GET", "/v1/responses/" + _seg(response_id))

    def stream_response(self, response_id: str, timeout: Optional[float] = None) -> Iterator[StreamEvent]:
        """``GET /v1/responses/{id}/stream`` -> replays the stored events, then
        follows a live response until ``response.completed``."""
        return self._stream("GET", "/v1/responses/" + _seg(response_id) + "/stream", timeout=timeout)

    def cancel(self, response_id: str) -> JSON:
        """``POST /v1/responses/{id}/cancel`` -> ``{"id", "status": "cancelled"}``."""
        return self._request("POST", "/v1/responses/" + _seg(response_id) + "/cancel")

    # -- sessions and models ----------------------------------------------

    def list_sessions(self) -> List[SessionSummary]:
        """``GET /v1/sessions`` -> newest first."""
        return _unwrap_list(self._request("GET", "/v1/sessions"))

    def get_session(self, session_id: str) -> SessionDetail:
        """``GET /v1/sessions/{id}`` -> the transcript (``messages``)."""
        return self._request("GET", "/v1/sessions/" + _seg(session_id))

    def rename_session(self, session_id: str, name: str) -> JSON:
        """``PATCH /v1/sessions/{id}`` body ``{"name"}``."""
        return self._request("PATCH", "/v1/sessions/" + _seg(session_id), json_body={"name": name})

    def delete_session(self, session_id: str) -> JSON:
        """``DELETE /v1/sessions/{id}``."""
        return self._request("DELETE", "/v1/sessions/" + _seg(session_id))

    def models(self) -> List[JSON]:
        """``GET /v1/models`` -> ``[{"id": "default"}, ...]``."""
        return _unwrap_list(self._request("GET", "/v1/models"))

    # -- files (paths relative to the persistent home) --------------------

    def list_files(self, path: str = ".") -> DirListing:
        """``GET /v1/files?path=`` -> ``{"path", "entries": [{"name", "type", "size", "modified_at"}]}``."""
        return self._request("GET", "/v1/files", query={"path": path})

    def read_file(self, path: str) -> bytes:
        """``GET /v1/files/content?path=`` -> the raw bytes."""
        return self._request("GET", "/v1/files/content", query={"path": path}, raw=True)

    def read_text(self, path: str, encoding: str = "utf-8") -> str:
        """:meth:`read_file` decoded as text."""
        return self.read_file(path).decode(encoding)

    def write_file(self, path: str, data: Union[bytes, str], content_type: Optional[str] = None) -> JSON:
        """``PUT /v1/files/content?path=`` with the raw body -> ``{"path", "size"}``. Creates parent directories."""
        if content_type is None:
            content_type = "text/plain; charset=utf-8" if isinstance(data, str) else "application/octet-stream"
        return self._request("PUT", "/v1/files/content", query={"path": path}, data=data, content_type=content_type)

    def delete_file(self, path: str) -> JSON:
        """``DELETE /v1/files?path=`` -> ``{"path", "deleted": true}``."""
        return self._request("DELETE", "/v1/files", query={"path": path})

    def move_file(self, src: str, dst: str) -> JSON:
        """``POST /v1/files/move`` body ``{"from", "to"}``."""
        return self._request("POST", "/v1/files/move", json_body={"from": src, "to": dst})

    def download(self, path: str = ".") -> bytes:
        """``GET /v1/files/download?path=`` -> a gzip tarball of the directory."""
        return self._request("GET", "/v1/files/download", query={"path": path}, raw=True)

    # -- exec -------------------------------------------------------------

    def exec(
        self,
        token: str,
        command: str,
        user: str = "",
        timeout_seconds: int = 0,
        workdir: str = "",
        env: Optional[Dict[str, str]] = None,
    ) -> ExecResult:
        """``POST /v1/exec`` with ``x-warren-instance-token: <token>`` -> exec result.

        The token is the per-instance secret only the daemon knows (available
        inside the instance as ``$WARREN_INSTANCE_TOKEN``); customers normally
        use :meth:`Warren.exec` instead.
        """
        body = _exec_body(command, user, timeout_seconds, workdir, env)
        return self._request(
            "POST",
            "/v1/exec",
            json_body=body,
            headers={HEADER_INSTANCE_TOKEN: token},
            timeout=_exec_timeout(self.timeout, timeout_seconds),
        )


# ---------------------------------------------------------------------------
# Private request builders
# ---------------------------------------------------------------------------


def _seg(value: str) -> str:
    """Percent-encode one path segment."""
    if not isinstance(value, str) or not value:
        raise WarrenError("invalid_request", "identifier must be a non-empty string")
    return urllib.parse.quote(value, safe="@")


def _inst(instance_id: str, *rest: str) -> str:
    path = "/v1/instances/" + _seg(instance_id)
    for part in rest:
        path += "/" + _seg(part)
    return path


def _exec_body(command: str, user: str, timeout_seconds: int, workdir: str, env: Optional[Dict[str, str]]) -> JSON:
    if not isinstance(command, str) or not command:
        raise WarrenError("invalid_request", "command must be a non-empty string")
    body: JSON = {"command": command}
    if user:
        body["user"] = user
    if timeout_seconds:
        body["timeout_seconds"] = int(timeout_seconds)
    if workdir:
        body["workdir"] = workdir
    if env:
        body["env"] = env
    return body


def _exec_timeout(client_timeout: Optional[float], timeout_seconds: int) -> Optional[float]:
    """Give the request at least the command's own timeout plus slack."""
    if client_timeout is None:
        return None
    if timeout_seconds and timeout_seconds + 30 > client_timeout:
        return float(timeout_seconds + 30)
    return client_timeout


def _responses_body(
    input: str,  # noqa: A002
    session_id: Optional[str],
    model: Optional[str],
    files: Optional[List[str]],
    metadata: Optional[JSON],
    stream: Optional[bool],
) -> JSON:
    if not isinstance(input, str):
        raise WarrenError("invalid_request", "input must be a string")
    return _compact(
        {"input": input, "session_id": session_id, "stream": stream, "model": model, "files": files, "metadata": metadata}
    )


# ---------------------------------------------------------------------------
# Demo: python3 warren.py (guarded by environment variables)
# ---------------------------------------------------------------------------


def _demo() -> int:
    api_url = os.environ.get("WARREN_API_URL")
    api_key = os.environ.get("WARREN_API_KEY")
    if not api_url or not api_key:
        print(
            "usage: WARREN_API_URL=http://api.warren.localhost:7337 WARREN_API_KEY=sk_live_... python3 warren.py\n"
            "optional: WARREN_INSTANCE_ID=<id> to show an instance's agent health;\n"
            "          WARREN_PROMPT='...' to stream one agent turn on that instance.",
            file=sys.stderr,
        )
        return 2
    try:
        w = Warren(api_url, api_key)
        health = w.health()
        print("daemon %s healthy=%s runtimes=%s" % (health.get("version"), health.get("healthy"), health.get("runtimes")))
        ws = w.workspace()
        limit = ws.get("awake_limit")
        print(
            "workspace %s (%s) awake %s/%s"
            % (ws.get("id"), ws.get("name"), ws.get("awake_count"), "unlimited" if limit is None else limit)
        )
        for inst in w.list_instances():
            print("  %s %-12s %-16s %s" % (inst.get("id"), inst.get("status"), inst.get("template"), inst.get("url")))
        instance_id = os.environ.get("WARREN_INSTANCE_ID")
        if instance_id:
            inst = w.get_instance(instance_id)
            agent = w.agent(inst)
            print("agent health: %s" % agent.health())
            prompt = os.environ.get("WARREN_PROMPT")
            if prompt and inst.get("agent_api"):
                for event, data in agent.stream(prompt):
                    if event == EVENT_DELTA:
                        sys.stdout.write(data.get("delta", "") if isinstance(data, dict) else str(data))
                        sys.stdout.flush()
                    elif event == EVENT_FAILED:
                        print("\nfailed: %s" % data)
                    elif event == EVENT_COMPLETED:
                        print("\n[%s]" % (data.get("status") if isinstance(data, dict) else data))
            elif prompt:
                print("instance %s has no Agent API (template without agent)" % instance_id)
        return 0
    except WarrenError as e:
        print("error: %s" % e, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(_demo())
