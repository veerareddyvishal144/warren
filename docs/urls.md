# URLs, signed URLs and cookies

Every instance is reachable at hostnames derived from its id. This page covers how requests to those hostnames are authenticated, how to let a browser in without the API key, and what the edge does on the way through.

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
ID=ab12cd34ef
```

## Hosts

| Host | Serves | Auth |
|---|---|---|
| `api.<base>` | Sandbox API under `/v1` | `Authorization: Bearer sk_live_...` or `X-Warren-Key: sk_live_...` |
| `<base>` | Dashboard; paths under `/v1/` go to the Sandbox API | browser |
| `<id>.<base>` | The instance's default port (3737, the gateway, on the built-in templates) | key, signed URL or cookie |
| `<id>-<port>.<base>` | Any port a process inside the instance listens on (the preview URL) | key, signed URL or cookie |

Locally `<base>` is `warren.localhost` and URLs carry port 7337: `http://ab12cd34ef.warren.localhost:7337/`, `http://ab12cd34ef-7681.warren.localhost:7337/`. In production it is your wildcard domain on 443 with no port in the URL. Ports in the `Host` header are ignored when classifying; a label with a second dot (`evil.ab12cd34ef.warren.localhost`) is not an instance host.

## Authenticating on an instance host

The edge checks, in this order:

1. `X-Warren-Key: sk_live_...` or `Authorization: Bearer sk_live_...`. The instance must belong to the key's workspace, else `404 not_found`.
2. `?warren_token=<token>` in the query string: a signed URL. The token's instance id and port must match the host (a token for port 0 means the template's default port). On success the edge sets the `warren_session` cookie and answers `302` to the same URL without the token.
3. The `warren_session` cookie, verified the same way.

Anything else is `401 invalid_api_key` with the standard JSON error body.

```bash
curl -s http://$ID.warren.localhost:7337/v1/health -H "X-Warren-Key: $WARREN_API_KEY"
curl -s http://$ID.warren.localhost:7337/v1/health -H "$AUTH"
curl -s http://$ID-8000.warren.localhost:7337/ -H "X-Warren-Key: $WARREN_API_KEY"
```

Use `X-Warren-Key` when the service inside the instance has its own `Authorization` scheme: the edge strips `X-Warren-Key` and forwards `Authorization` untouched.

## Signed URLs

Browsers, iframes, webhooks and people you share a link with cannot send the key header. Mint a URL that carries its own credential:

```bash
curl -s "$WARREN_API/instances/$ID/signed-url?port=7681&ttl=3600&path=/" -H "$AUTH"
# {"url":"http://ab12cd34ef-7681.warren.localhost:7337/?warren_token=YWIxMmNkMzRlZnw3NjgxfDE3NjMwMDM2MDA.8f3c...","expires_at":1763003600}
```

| Parameter | Meaning | Default and range |
|---|---|---|
| `port` | Port the URL points at; omitted means the template's default port, and the URL is then `<id>.<base>` rather than `<id>-<port>.<base>` | default port |
| `ttl` | Lifetime in seconds | 3600; 60 to 86400 |
| `path` | Path and query the URL opens | `/`; at most 2048 characters |

The token is `base64url(payload).base64url(HMAC-SHA256(secret, payload))` with payload `<instance_id>|<port>|<expires_at>`, signed with the daemon's secret (`<data>/secret`, or `--secret`). It is a bearer credential: anyone holding the URL can use it until `expires_at`, and there is no revocation short of rotating the daemon secret (which invalidates every outstanding token and cookie). Choose the shortest `ttl` that works and prefer `path` over `?warren_token=` in links you keep.

What a browser sees:

```bash
curl -si "http://$ID-7681.warren.localhost:7337/?warren_token=..." | head -5
# HTTP/1.1 302 Found
# location: http://ab12cd34ef-7681.warren.localhost:7337/
# set-cookie: warren_session=<token>; HttpOnly; SameSite=Lax; Path=/
```

Then the clean URL works on that host for as long as the token is valid:

```bash
curl -s -c jar.txt "http://$ID-7681.warren.localhost:7337/?warren_token=..." -o /dev/null
curl -s -b jar.txt http://$ID-7681.warren.localhost:7337/ | head -3
```

The cookie is scoped to the host that set it, so a signed URL for `<id>-7681` does not open `<id>-8000` or `<id>`; mint one per port. It is `HttpOnly` (not readable by page scripts) and `SameSite=Lax`.

Typical uses:

- **Browser terminal**: `port=7681` (ttyd). The dashboard's terminal button does exactly this.
- **Preview a web app**: run it on any port inside the instance, mint `port=<that port>`, open or iframe the URL.
- **Give an end user their computer**: your backend holds the workspace key, mints a short-lived signed URL per visit, and the user never sees a key.

## Preview URLs

`<id>-<port>.<base>` reaches any TCP port inside the instance; nothing has to be declared in the template. The URL always routes:

| Situation | Answer |
|---|---|
| Nothing listens on the port | `502 bad_gateway`, plain-text body |
| Template `default_port` is 0 and the URL has no port suffix | `502`, text `this template has no default port` |
| Instance is `sleeping` | held while it wakes, then proxied |
| Instance is `stopped` | `409 conflict` |
| Instance belongs to another workspace, or unknown id | `404 not_found` |

## What the edge does to a proxied request

- Wakes a sleeping instance first and records activity on every request and response (this is what resets the idle timer).
- Proxies to the instance IP and port, preserving path and query, setting `X-Forwarded-Host` and `X-Forwarded-Proto`.
- Strips `X-Warren-Key`; strips the `warren_session` cookie from the forwarded `Cookie` header; keeps `Authorization` for the instance's own use.
- Passes WebSocket upgrades through (ttyd and live-reload dev servers work).
- Answers CORS preflights on instance hosts with `Access-Control-Allow-Origin: <request Origin>`, `Access-Control-Allow-Headers: X-Warren-Key, Content-Type, Authorization`, `Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS`, `Access-Control-Allow-Credentials: true`, and adds `Access-Control-Allow-Origin` to proxied responses when an `Origin` is present. The API host does the same, which is how the dashboard calls it cross-origin.

A page served from an instance can therefore call the Agent API on another instance, or the Sandbox API, with `fetch` and the key header, or rely on the cookie with `credentials: "include"` on its own host.

## Production domains

Point `*.warren.example.com` and `warren.example.com` at the host, terminate TLS with a wildcard certificate in front of the daemon, and start it with `--base-domain warren.example.com --public-scheme https --public-port 443`. The daemon then prints `https://ab12cd34ef.warren.example.com` as the instance `url` and signs URLs for that host. Details in [self-hosting.md](self-hosting.md).
