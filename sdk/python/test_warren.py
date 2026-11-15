"""Offline tests for warren.py: URL building, headers, bodies, error parsing
and SSE parsing. Nothing here opens a socket; urllib and http.client are mocked.

Run:  python3 -m unittest -v test_warren
"""

import io
import json
import socket
import unittest
import urllib.error
import urllib.parse
from unittest import mock

import warren
from warren import Agent, Warren, WarrenError, parse_sse

API_URL = "http://api.warren.localhost:7337"
KEY = "sk_live_" + "A" * 40


class FakeResponse:
    """What a mocked ``urllib.request.urlopen`` returns."""

    def __init__(self, status=200, body=b"{}", content_type="application/json"):
        self.status = status
        self._body = body
        self.headers = {"Content-Type": content_type}

    def read(self):
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def json_response(payload, status=200):
    return FakeResponse(status, json.dumps(payload).encode("utf-8"))


class UrlopenRecorder:
    """Replaces ``urllib.request.urlopen``; records requests, replays answers."""

    def __init__(self, *answers):
        self.answers = list(answers)
        self.requests = []
        self.timeouts = []

    def __call__(self, req, timeout=None):
        self.requests.append(req)
        self.timeouts.append(timeout)
        answer = self.answers.pop(0) if self.answers else json_response({})
        if isinstance(answer, Exception):
            raise answer
        return answer

    @property
    def last(self):
        return self.requests[-1]


def headers_of(req):
    return {k.lower(): v for k, v in req.header_items()}


def http_error(status, body, content_type="application/json"):
    headers = {"Content-Type": content_type}
    return urllib.error.HTTPError("http://x", status, "status text", headers, io.BytesIO(body))


class SandboxApiTests(unittest.TestCase):
    def setUp(self):
        self.recorder = UrlopenRecorder()
        patcher = mock.patch.object(warren.urllib.request, "urlopen", self.recorder)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.w = Warren(API_URL, KEY)

    def test_base_url_normalisation(self):
        for given in (API_URL, API_URL + "/", API_URL + "/v1", API_URL + "/v1/"):
            self.assertEqual(Warren(given, KEY).base_url, API_URL, given)
        with self.assertRaises(WarrenError) as ctx:
            Warren("api.warren.localhost", KEY)
        self.assertEqual(ctx.exception.code, "invalid_request")

    def test_get_instance_url_method_and_headers(self):
        self.recorder.answers = [json_response({"id": "ab12cd34ef", "status": "running"})]
        inst = self.w.get_instance("ab12cd34ef")
        req = self.recorder.last
        self.assertEqual(req.full_url, API_URL + "/v1/instances/ab12cd34ef")
        self.assertEqual(req.get_method(), "GET")
        h = headers_of(req)
        self.assertEqual(h["authorization"], "Bearer " + KEY)
        self.assertEqual(h["accept"], "application/json")
        self.assertNotIn("x-warren-key", h)
        self.assertIsNone(req.data)
        self.assertEqual(inst["status"], "running")
        self.assertEqual(self.recorder.timeouts[-1], 60.0)

    def test_create_instance_body_drops_absent_fields(self):
        self.recorder.answers = [json_response({"id": "ab12cd34ef", "status": "provisioning"})]
        self.w.create_instance(template="warren-agent", type="vm", resources={"cpu": 2, "disk": 8}, auto_sleep=True)
        req = self.recorder.last
        self.assertEqual(req.get_method(), "POST")
        self.assertEqual(req.full_url, API_URL + "/v1/instances")
        self.assertEqual(headers_of(req)["content-type"], "application/json")
        self.assertEqual(
            json.loads(req.data.decode()),
            {"template": "warren-agent", "type": "vm", "resources": {"cpu": 2, "disk": 8}, "auto_sleep": True},
        )

    def test_action_endpoints_post_without_body(self):
        for method, suffix in (
            (self.w.stop_instance, "stop"),
            (self.w.start_instance, "start"),
            (self.w.sleep_instance, "sleep"),
            (self.w.restart_instance, "restart"),
        ):
            self.recorder.answers = [json_response({"id": "ab12cd34ef", "status": suffix})]
            method("ab12cd34ef")
            req = self.recorder.last
            self.assertEqual(req.get_method(), "POST")
            self.assertEqual(req.full_url, API_URL + "/v1/instances/ab12cd34ef/" + suffix)
            self.assertIsNone(req.data)

    def test_optional_bodies_are_objects(self):
        self.recorder.answers = [json_response({"id": "b" * 32})]
        self.w.create_backup("ab12cd34ef")
        self.assertEqual(json.loads(self.recorder.last.data.decode()), {})
        self.recorder.answers = [json_response({"id": "ab12cd34ef", "status": "updating"})]
        self.w.update_instance("ab12cd34ef", template="my-agent@3")
        self.assertEqual(json.loads(self.recorder.last.data.decode()), {"template": "my-agent@3"})
        self.recorder.answers = [json_response({"workspace": {}, "api_key": "sk_live_x", "key_id": "k", "wallet_micros": 5000000})]
        Warren(API_URL, None).signup()
        self.assertEqual(json.loads(self.recorder.last.data.decode()), {})
        self.assertNotIn("authorization", headers_of(self.recorder.last))

    def test_query_building(self):
        self.recorder.answers = [json_response({"logs": "hello"})]
        self.assertEqual(self.w.logs("ab12cd34ef", tail=200, since=1763000000), "hello")
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/instances/ab12cd34ef/logs?tail=200&since=1763000000")

        self.recorder.answers = [json_response({"logs": ""})]
        self.w.logs("ab12cd34ef")
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/instances/ab12cd34ef/logs")

        self.recorder.answers = [json_response({"url": "http://x", "expires_at": 1})]
        self.w.signed_url("ab12cd34ef", port=7681, ttl=3600, path="/a b")
        self.assertEqual(
            self.recorder.last.full_url, API_URL + "/v1/instances/ab12cd34ef/signed-url?port=7681&ttl=3600&path=/a%20b"
        )

        self.recorder.answers = [json_response({"data": []})]
        self.w.ledger(limit=5)
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/billing/ledger?limit=5")

    def test_path_segments_are_encoded(self):
        self.recorder.answers = [json_response({"name": "my-agent", "revision": 2})]
        self.w.get_template("my-agent@2")
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/templates/my-agent@2")
        self.recorder.answers = [json_response({})]
        self.w.delete_cron("ab12cd34ef", "c d")
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/instances/ab12cd34ef/crons/c%20d")
        with self.assertRaises(WarrenError):
            self.w.get_instance("")

    def test_lists_are_unwrapped(self):
        self.recorder.answers = [json_response({"data": [{"id": "a"}, {"id": "b"}]})]
        self.assertEqual([i["id"] for i in self.w.list_instances()], ["a", "b"])
        self.recorder.answers = [json_response({"data": [{"at": 1, "cpu_percent": 0.5}]})]
        self.assertEqual(self.w.metrics("ab12cd34ef", hours=2)[0]["at"], 1)
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/instances/ab12cd34ef/metrics?hours=2")

    def test_exec_body_and_timeout(self):
        self.recorder.answers = [json_response({"exit_code": 0, "stdout": "hi\n", "stderr": "", "duration_ms": 3})]
        result = self.w.exec("ab12cd34ef", "echo hi", user="root", timeout_seconds=120, env={"A": "1"})
        self.assertEqual(result["stdout"], "hi\n")
        self.assertEqual(
            json.loads(self.recorder.last.data.decode()),
            {"command": "echo hi", "user": "root", "timeout_seconds": 120, "env": {"A": "1"}},
        )
        self.assertEqual(self.recorder.timeouts[-1], 150.0)
        with self.assertRaises(WarrenError) as ctx:
            self.w.exec("ab12cd34ef", "")
        self.assertEqual(ctx.exception.code, "invalid_request")

    def test_patch_and_cron_bodies(self):
        self.recorder.answers = [json_response({"id": "ab12cd34ef"})]
        self.w.patch_instance("ab12cd34ef", auto_sleep=False, idle_timeout_seconds=600, env={})
        self.assertEqual(self.recorder.last.get_method(), "PATCH")
        self.assertEqual(
            json.loads(self.recorder.last.data.decode()), {"auto_sleep": False, "idle_timeout_seconds": 600, "env": {}}
        )
        self.recorder.answers = [json_response({"id": "c" * 32})]
        self.w.create_cron("ab12cd34ef", "report", "0 9 * * 1-5", timezone="Europe/Berlin")
        self.assertEqual(
            json.loads(self.recorder.last.data.decode()),
            {"prompt": "report", "schedule": "0 9 * * 1-5", "timezone": "Europe/Berlin"},
        )
        self.recorder.answers = [json_response({"name": "x", "revision": 1})]
        self.w.register_template("x-agent", "ghcr.io/me/x:1", default_port=8080, agent=True, env={"K": "v"})
        body = json.loads(self.recorder.last.data.decode())
        self.assertEqual(body["name"], "x-agent")
        self.assertEqual(body["image_ref"], "ghcr.io/me/x:1")
        self.assertEqual(body["default_port"], 8080)
        self.assertTrue(body["agent"])
        self.assertEqual(body["env"], {"K": "v"})

    def test_error_envelope(self):
        body = json.dumps(
            {"error": {"code": "conflict", "message": "instance is stopped", "details": {"status": "stopped"}}}
        ).encode()
        self.recorder.answers = [http_error(409, body)]
        with self.assertRaises(WarrenError) as ctx:
            self.w.exec("ab12cd34ef", "true")
        e = ctx.exception
        self.assertEqual((e.code, e.status, e.message), ("conflict", 409, "instance is stopped"))
        self.assertEqual(e.details, {"status": "stopped"})
        self.assertEqual(str(e), "conflict (HTTP 409): instance is stopped")

    def test_error_without_envelope(self):
        self.recorder.answers = [http_error(502, b"this template has no default port\n", "text/plain")]
        with self.assertRaises(WarrenError) as ctx:
            self.w.get_instance("ab12cd34ef")
        self.assertEqual(ctx.exception.code, "bad_gateway")
        self.assertEqual(ctx.exception.status, 502)
        self.assertEqual(ctx.exception.message, "HTTP 502: this template has no default port")

        self.recorder.answers = [http_error(404, b'{"code":"not_found","message":"instance not found"}')]
        with self.assertRaises(WarrenError) as ctx:
            self.w.get_instance("ab12cd34ef")
        self.assertEqual(ctx.exception.code, "not_found")

        self.recorder.answers = [http_error(401, b'{"unexpected": true}')]
        with self.assertRaises(WarrenError) as ctx:
            self.w.workspace()
        self.assertEqual(ctx.exception.code, "invalid_api_key")

        self.recorder.answers = [http_error(418, b"")]
        with self.assertRaises(WarrenError) as ctx:
            self.w.workspace()
        self.assertEqual((ctx.exception.code, ctx.exception.message), ("internal", "HTTP 418"))

    def test_transport_errors(self):
        self.recorder.answers = [urllib.error.URLError(ConnectionRefusedError(111, "Connection refused"))]
        with self.assertRaises(WarrenError) as ctx:
            self.w.health()
        self.assertEqual(ctx.exception.code, "try_again")
        self.assertEqual(ctx.exception.status, 0)
        self.assertIn("/v1/health", ctx.exception.message)

        self.recorder.answers = [urllib.error.URLError(socket.timeout("timed out"))]
        with self.assertRaises(WarrenError) as ctx:
            self.w.health()
        self.assertEqual(ctx.exception.code, "timeout")

        self.recorder.answers = [socket.timeout("timed out")]
        with self.assertRaises(WarrenError) as ctx:
            self.w.health()
        self.assertEqual(ctx.exception.code, "timeout")

    def test_bad_json_on_success(self):
        self.recorder.answers = [FakeResponse(200, b"<html>", "text/html")]
        with self.assertRaises(WarrenError) as ctx:
            self.w.workspace()
        self.assertEqual(ctx.exception.code, "internal")
        self.assertEqual(ctx.exception.status, 200)

    def test_wait_for(self):
        self.recorder.answers = [
            json_response({"id": "ab12cd34ef", "status": "provisioning"}),
            json_response({"id": "ab12cd34ef", "status": "starting"}),
            json_response({"id": "ab12cd34ef", "status": "running"}),
        ]
        with mock.patch.object(warren.time, "sleep") as sleep:
            inst = self.w.wait_for("ab12cd34ef", "running", timeout=30, poll=0.5)
        self.assertEqual(inst["status"], "running")
        self.assertEqual(sleep.call_count, 2)

        self.recorder.answers = [json_response({"id": "ab12cd34ef", "status": "failed", "status_reason": "image pull failed"})]
        with self.assertRaises(WarrenError) as ctx:
            self.w.wait_for("ab12cd34ef", ("running", "sleeping"))
        self.assertEqual(ctx.exception.code, "conflict")
        self.assertIn("image pull failed", ctx.exception.message)

        self.recorder.answers = [json_response({"id": "ab12cd34ef", "status": "starting"})] * 5
        with mock.patch.object(warren.time, "sleep"), mock.patch.object(warren.time, "monotonic", side_effect=[0, 0, 100, 200]):
            with self.assertRaises(WarrenError) as ctx:
                self.w.wait_for("ab12cd34ef", "running", timeout=50)
        self.assertEqual(ctx.exception.code, "timeout")

    def test_instance_url_and_agent_factory(self):
        self.assertEqual(self.w.instance_url("ab12cd34ef"), "http://ab12cd34ef.warren.localhost:7337")
        self.assertEqual(self.w.instance_url("ab12cd34ef", 7681), "http://ab12cd34ef-7681.warren.localhost:7337")
        self.assertEqual(Warren("https://api.example.com", KEY).instance_url("ab12cd34ef"), "https://ab12cd34ef.example.com")

        agent = self.w.agent({"id": "ab12cd34ef", "url": "http://ab12cd34ef.warren.localhost:7337"})
        self.assertIsInstance(agent, Agent)
        self.assertEqual(agent.base_url, "http://ab12cd34ef.warren.localhost:7337")
        self.assertEqual(agent.api_key, KEY)

        self.recorder.answers = [json_response({"id": "ab12cd34ef", "url": "http://ab12cd34ef.warren.localhost:7337"})]
        self.assertEqual(self.w.agent("ab12cd34ef").base_url, "http://ab12cd34ef.warren.localhost:7337")
        self.assertEqual(self.recorder.last.full_url, API_URL + "/v1/instances/ab12cd34ef")
        self.assertEqual(self.w.agent("http://other:3737/").base_url, "http://other:3737")
        with self.assertRaises(WarrenError):
            self.w.agent({"id": "ab12cd34ef"})


class AgentApiTests(unittest.TestCase):
    URL = "http://ab12cd34ef.warren.localhost:7337"

    def setUp(self):
        self.recorder = UrlopenRecorder()
        patcher = mock.patch.object(warren.urllib.request, "urlopen", self.recorder)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.agent = Agent(self.URL, KEY)

    def test_key_header_and_respond_body(self):
        self.recorder.answers = [
            json_response({"id": "r1", "session_id": "s1", "status": "completed", "output_text": "echo: hi", "created_at": 1})
        ]
        resp = self.agent.respond("hi", session_id="s1", metadata={"k": 1})
        req = self.recorder.last
        self.assertEqual(req.full_url, self.URL + "/v1/responses")
        h = headers_of(req)
        self.assertEqual(h["x-warren-key"], KEY)
        self.assertNotIn("authorization", h)
        self.assertEqual(json.loads(req.data.decode()), {"input": "hi", "session_id": "s1", "metadata": {"k": 1}})
        self.assertEqual(resp["output_text"], "echo: hi")
        self.assertEqual(self.recorder.timeouts[-1], 600.0)

    def test_files(self):
        self.recorder.answers = [json_response({"path": "notes", "entries": [{"name": "a b.md", "type": "file", "size": 3, "modified_at": 1}]})]
        listing = self.agent.list_files("notes")
        self.assertEqual(self.recorder.last.full_url, self.URL + "/v1/files?path=notes")
        self.assertEqual(listing["entries"][0]["name"], "a b.md")

        self.recorder.answers = [FakeResponse(200, b"hello", "text/plain")]
        self.assertEqual(self.agent.read_file("notes/a b.md"), b"hello")
        self.assertEqual(self.recorder.last.full_url, self.URL + "/v1/files/content?path=notes/a%20b.md")
        self.assertEqual(headers_of(self.recorder.last)["accept"], "*/*")

        self.recorder.answers = [json_response({"path": "notes/c.txt", "size": 5})]
        self.agent.write_file("notes/c.txt", "hello")
        req = self.recorder.last
        self.assertEqual(req.get_method(), "PUT")
        self.assertEqual(req.full_url, self.URL + "/v1/files/content?path=notes/c.txt")
        self.assertEqual(req.data, b"hello")
        self.assertEqual(headers_of(req)["content-type"], "text/plain; charset=utf-8")

        self.recorder.answers = [json_response({"path": "x.bin", "size": 2})]
        self.agent.write_file("x.bin", b"\x00\x01")
        self.assertEqual(headers_of(self.recorder.last)["content-type"], "application/octet-stream")

        self.recorder.answers = [json_response({"path": "x.bin", "deleted": True})]
        self.agent.delete_file("x.bin")
        self.assertEqual(self.recorder.last.get_method(), "DELETE")
        self.assertEqual(self.recorder.last.full_url, self.URL + "/v1/files?path=x.bin")

        self.recorder.answers = [json_response({"path": "b"})]
        self.agent.move_file("a", "b")
        self.assertEqual(self.recorder.last.full_url, self.URL + "/v1/files/move")
        self.assertEqual(json.loads(self.recorder.last.data.decode()), {"from": "a", "to": "b"})

        self.recorder.answers = [FakeResponse(200, b"\x1f\x8b", "application/gzip")]
        self.assertEqual(self.agent.download("notes"), b"\x1f\x8b")
        self.assertEqual(self.recorder.last.full_url, self.URL + "/v1/files/download?path=notes")

    def test_sessions_and_models(self):
        self.recorder.answers = [json_response({"data": [{"id": "s1", "name": "", "message_count": 2}]})]
        self.assertEqual(self.agent.list_sessions()[0]["id"], "s1")
        self.recorder.answers = [json_response({"id": "s1", "name": "renamed"})]
        self.agent.rename_session("s1", "renamed")
        self.assertEqual(self.recorder.last.get_method(), "PATCH")
        self.assertEqual(json.loads(self.recorder.last.data.decode()), {"name": "renamed"})
        self.recorder.answers = [json_response({"data": [{"id": "default"}]})]
        self.assertEqual(self.agent.models(), [{"id": "default"}])
        self.recorder.answers = [json_response({"id": "r1", "status": "cancelled"})]
        self.agent.cancel("r1")
        self.assertEqual(self.recorder.last.full_url, self.URL + "/v1/responses/r1/cancel")

    def test_exec_sends_instance_token(self):
        self.recorder.answers = [json_response({"exit_code": 0, "stdout": "", "stderr": "", "duration_ms": 1})]
        self.agent.exec("tok123", "uname -a", workdir="/home/agent")
        h = headers_of(self.recorder.last)
        self.assertEqual(h["x-warren-instance-token"], "tok123")
        self.assertEqual(h["x-warren-key"], KEY)
        self.assertEqual(json.loads(self.recorder.last.data.decode()), {"command": "uname -a", "workdir": "/home/agent"})

    def test_not_supported_error(self):
        body = json.dumps({"error": {"code": "not_supported", "message": "this template has no agent"}}).encode()
        self.recorder.answers = [http_error(404, body)]
        with self.assertRaises(WarrenError) as ctx:
            self.agent.respond("hi")
        self.assertEqual((ctx.exception.code, ctx.exception.status), ("not_supported", 404))


SAMPLE_STREAM = (
    b": keep-alive comment\n"
    b"event: response.created\n"
    b'data: {"id": "r1", "session_id": "s1"}\n'
    b"\n"
    b"event: response.output_text.delta\r\n"
    b'data: {"delta": "Hel"}\r\n'
    b"\r\n"
    b"event: response.output_text.delta\n"
    b'data:{"delta": "lo"}\n'
    b"\n"
    b"data: line one\n"
    b"data: line two\n"
    b"\n"
    b"event: ignored.without.data\n"
    b"\n"
    b"id: 7\n"
    b"retry: 1000\n"
    b"event: response.completed\n"
    b'data: {"id": "r1", "session_id": "s1",\n'
    b'data:  "output_text": "Hello", "status": "completed"}\n'
    b"\n"
    b"event: response.failed\n"
    b"data: not json\n"
)

EXPECTED_EVENTS = [
    ("response.created", {"id": "r1", "session_id": "s1"}),
    ("response.output_text.delta", {"delta": "Hel"}),
    ("response.output_text.delta", {"delta": "lo"}),
    ("message", "line one\nline two"),
    ("response.completed", {"id": "r1", "session_id": "s1", "output_text": "Hello", "status": "completed"}),
]


class SseParsingTests(unittest.TestCase):
    def test_whole_buffer(self):
        self.assertEqual(list(parse_sse([SAMPLE_STREAM])), EXPECTED_EVENTS)

    def test_byte_at_a_time_and_odd_chunks(self):
        self.assertEqual(list(parse_sse(bytes([b]) for b in SAMPLE_STREAM)), EXPECTED_EVENTS)
        for size in (2, 3, 7, 13, 64):
            chunks = [SAMPLE_STREAM[i : i + size] for i in range(0, len(SAMPLE_STREAM), size)]
            self.assertEqual(list(parse_sse(chunks)), EXPECTED_EVENTS, size)

    def test_cr_only_line_endings_and_split_crlf(self):
        stream = b'event: a\rdata: {"n": 1}\r\r' b'event: b\r\ndata: {"n": 2}\r\n\r\n'
        self.assertEqual(list(parse_sse([stream])), [("a", {"n": 1}), ("b", {"n": 2})])
        # \r\n split across chunks must not produce an extra empty line
        first, second = stream.split(b"\n", 1)
        self.assertEqual(list(parse_sse([first + b"\n", second])), [("a", {"n": 1}), ("b", {"n": 2})])
        self.assertEqual(list(parse_sse([first, b"\n" + second])), [("a", {"n": 1}), ("b", {"n": 2})])

    def test_unicode_split_across_chunks(self):
        payload = json.dumps({"delta": "héllo wörld"}, ensure_ascii=False).encode("utf-8")
        stream = b"event: response.output_text.delta\ndata: " + payload + b"\n\n"
        cut = stream.index("é".encode("utf-8")) + 1  # inside the two-byte sequence
        self.assertEqual(list(parse_sse([stream[:cut], stream[cut:]])), [("response.output_text.delta", {"delta": "héllo wörld"})])

    def test_empty_stream(self):
        self.assertEqual(list(parse_sse([])), [])
        self.assertEqual(list(parse_sse([b"", b"\n\n", b": only comments\n"])), [])


class FakeHttpResponse:
    def __init__(self, status, chunks, content_type="text/event-stream"):
        self.status = status
        self._chunks = list(chunks)
        self._ctype = content_type

    def getheader(self, name, default=None):
        return self._ctype if name.lower() == "content-type" else default

    def read1(self, n=-1):
        return self._chunks.pop(0) if self._chunks else b""

    def read(self):
        data = b"".join(self._chunks)
        self._chunks = []
        return data


class FakeHttpConnection:
    """Stand-in for ``http.client.HTTPConnection``; the class records the last instance."""

    last = None
    response = None

    def __init__(self, host, port=None, timeout=None):
        self.host, self.port, self.timeout = host, port, timeout
        self.closed = False
        self.requested = None
        FakeHttpConnection.last = self

    def request(self, method, url, body=None, headers=None):
        self.requested = (method, url, body, dict(headers or {}))

    def getresponse(self):
        return FakeHttpConnection.response

    def close(self):
        self.closed = True


class AgentStreamTests(unittest.TestCase):
    URL = "http://ab12cd34ef.warren.localhost:7337"

    def setUp(self):
        patcher = mock.patch.object(warren.http.client, "HTTPConnection", FakeHttpConnection)
        patcher.start()
        self.addCleanup(patcher.stop)
        FakeHttpConnection.last = None
        self.agent = Agent(self.URL, KEY)

    def test_stream_request_and_events(self):
        FakeHttpConnection.response = FakeHttpResponse(200, [SAMPLE_STREAM[:40], SAMPLE_STREAM[40:]])
        events = list(self.agent.stream("hi", session_id="s1", timeout=5))
        self.assertEqual(events, EXPECTED_EVENTS)
        conn = FakeHttpConnection.last
        self.assertEqual((conn.host, conn.port, conn.timeout), ("ab12cd34ef.warren.localhost", 7337, 5))
        method, target, body, headers = conn.requested
        self.assertEqual((method, target), ("POST", "/v1/responses"))
        lower = {k.lower(): v for k, v in headers.items()}
        self.assertEqual(lower["x-warren-key"], KEY)
        self.assertEqual(lower["accept"], "text/event-stream")
        self.assertEqual(lower["content-type"], "application/json")
        self.assertEqual(json.loads(body.decode()), {"input": "hi", "session_id": "s1", "stream": True})
        self.assertTrue(conn.closed)

    def test_stream_replay_uses_get(self):
        FakeHttpConnection.response = FakeHttpResponse(200, [b'event: response.completed\ndata: {"id":"r1"}\n\n'])
        self.assertEqual(list(self.agent.stream_response("r1")), [("response.completed", {"id": "r1"})])
        method, target, body, _ = FakeHttpConnection.last.requested
        self.assertEqual((method, target, body), ("GET", "/v1/responses/r1/stream", None))

    def test_stream_error_envelope(self):
        body = json.dumps({"error": {"code": "not_supported", "message": "no agent"}}).encode()
        FakeHttpConnection.response = FakeHttpResponse(404, [body], "application/json")
        with self.assertRaises(WarrenError) as ctx:
            self.agent.stream("hi")
        self.assertEqual((ctx.exception.code, ctx.exception.status, ctx.exception.message), ("not_supported", 404, "no agent"))
        self.assertTrue(FakeHttpConnection.last.closed)

    def test_stream_wrong_content_type(self):
        FakeHttpConnection.response = FakeHttpResponse(200, [b'{"id": "r1"}'], "application/json")
        with self.assertRaises(WarrenError) as ctx:
            self.agent.stream("hi")
        self.assertEqual(ctx.exception.code, "internal")
        self.assertIn("text/event-stream", ctx.exception.message)

    def test_closing_the_generator_closes_the_connection(self):
        FakeHttpConnection.response = FakeHttpResponse(200, [SAMPLE_STREAM])
        gen = self.agent.stream("hi")
        self.assertEqual(next(gen), EXPECTED_EVENTS[0])
        self.assertFalse(FakeHttpConnection.last.closed)
        gen.close()
        self.assertTrue(FakeHttpConnection.last.closed)

    def test_https_uses_tls_connection(self):
        with mock.patch.object(warren.http.client, "HTTPSConnection", FakeHttpConnection):
            FakeHttpConnection.response = FakeHttpResponse(200, [b"event: x\ndata: 1\n\n"])
            self.assertEqual(list(Agent("https://ab12cd34ef.example.com", KEY).stream("hi")), [("x", 1)])
            self.assertEqual((FakeHttpConnection.last.host, FakeHttpConnection.last.port), ("ab12cd34ef.example.com", None))


if __name__ == "__main__":
    unittest.main()
