# anthroxy

A small, transparent reverse proxy that sits in front of `api.anthropic.com` and absorbs
Anthropic's burst / "acceleration" rate limit on behalf of a fleet of clients.

## Why

When you run many Claude Code agents (or any Anthropic API clients) on one host, they hit a
shared, account-level throttle on the *rate at which new requests start* — the "acceleration"
limit — independent of token usage. Each client retries in isolation: they don't know about
each other, so they don't coordinate backoff, and a thundering-herd of independent retries
makes the throttling worse, not better.

anthroxy puts a single coordination point in the path. Every client points
`ANTHROPIC_BASE_URL` at it, and the proxy enforces one fleet-wide concurrency cap, paces how
fast new requests start, and does the retry/backoff *once*, centrally, honoring upstream
`Retry-After`. The clients just see their request eventually succeed.

It is **transparent**: it holds no credentials of its own. Each request's `Authorization`
header (your Claude subscription OAuth token, an API key, whatever) is forwarded upstream
verbatim, so subscription billing and auth are preserved exactly. A request with no valid
token just gets the upstream `401` back. The body is never decoded or rewritten.

## How it works

The core is a literal match on the upstream HTTP status:

- **200** → stream the response straight back, unbuffered (SSE passes through intact).
- **429** → honor `Retry-After`, then exponential backoff with jitter, and retry.
- **500–599** → same backoff/retry.
- **anything else** → pass through unchanged.

Two in-memory gates shape the load before a request is even sent upstream:

- a **semaphore** caps concurrent in-flight requests (`ANTHROXY_MAX_CONCURRENCY`), and
- a **minimum interval** between request *starts* (`ANTHROXY_MIN_INTERVAL_MS`) paces the
  rate-of-onset that the acceleration limit actually keys on.

Because that state lives in memory, anthroxy runs as a **single instance** — a second replica
would split the budget and double the effective concurrency against Anthropic.

Built in Rust (tokio + hyper + reqwest/rustls). Headers are forwarded verbatim (only
hop-by-hop `host`/`connection`/`content-length` are dropped); TLS roots are bundled via
rustls, so the runtime image needs no system CA bundle.

## Configuration

All via environment variables:

| Variable                   | Default                    | Purpose                                          |
| -------------------------- | -------------------------- | ------------------------------------------------ |
| `ANTHROXY_BIND`            | `127.0.0.1`                | Listen address (loopback by default — see below) |
| `ANTHROXY_PORT`            | `8787`                     | Listen port                                      |
| `ANTHROXY_UPSTREAM`        | `https://api.anthropic.com`| Upstream base URL                                |
| `ANTHROXY_MAX_CONCURRENCY` | `3`                        | Max concurrent in-flight requests                |
| `ANTHROXY_MIN_INTERVAL_MS` | `350`                      | Minimum gap between request starts               |
| `ANTHROXY_MAX_RETRIES`     | `8`                        | Retry attempts on 429 / 5xx                      |
| `ANTHROXY_BASE_BACKOFF_MS` | `500`                      | Base for exponential backoff                     |

The concurrency and pacing defaults (`3` and `350ms`) are deliberately **conservative starting
points, not a measured threshold** — Anthropic does not publish the acceleration-limit ceiling,
and probing for it means intentionally tripping the throttle. The intended way to tune them is
empirical from real traffic: anthroxy logs every request as `{method} {path} -> {status}
(attempt {n})`, so watch for `429 (attempt …)` lines under load and loosen the knobs from there.

The bind defaults to loopback on purpose: anthroxy is an open relay to Anthropic for whoever
can reach it, so it should never listen on a wildcard. Front it with a reverse proxy (e.g.
Caddy) if you want it reachable under a hostname, and keep the proxy itself on `127.0.0.1`.

## Usage

```sh
anthroxy &
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude -p "hello"
```

For Claude Code fleet-wide, set it in `settings.json` (there is no dedicated key — the
`env` block is the canonical place):

```json
{ "env": { "ANTHROPIC_BASE_URL": "https://anthroxy.your.domain" } }
```

## Deploy

The `Dockerfile` produces a distroless image (~29 MB). `anthroxy.nomad.hcl` runs it as a
single-instance Nomad/podman service, bound to loopback, fronted by Caddy.
