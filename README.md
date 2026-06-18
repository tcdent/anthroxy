# anthroxy

A small, transparent reverse proxy in front of `api.anthropic.com` that **keeps Claude Code
agents alive through rate-limit (429) and server (5xx) errors** so they don't halt and need a
human to restart them.

## Why

Claude Code does **not** ride out a `429` on its own — when Anthropic returns its burst /
"acceleration" throttle (the *"Server is temporarily limiting requests (not your usage limit)"*
error), the agent errors out and **stops**, and someone has to come back and poke it to continue.
Run a fleet of agents and that's a steady stream of babysitting.

anthroxy makes the throttle invisible. Every client points `ANTHROPIC_BASE_URL` at it; when
upstream returns `429` or `5xx`, the proxy **retries — honoring `Retry-After`, with exponential
backoff in between — for up to a wall-clock window (default 10 minutes)**, so a transient throttle
or blip never surfaces to the agent. The agent just sees its request eventually succeed.

It deliberately does **not** try to *anticipate* the limit. An earlier version capped concurrency
and paced request starts; measurement (see below) showed request-concurrency simply isn't the
constraint — 100+ parallel agents run fine — so all of that was overreach. The honest design is:
**react to the 429, don't predict it.**

It is **transparent**: it holds no credentials. Each request's `Authorization` header (your Claude
subscription OAuth token, an API key, whatever) is forwarded upstream verbatim, so billing and auth
are preserved exactly. A request with no valid token just gets the upstream `401` back. The body is
never decoded or rewritten (that's where re-serializers corrupt `anthropic-beta` features).

## How it works

The core is a literal match on the upstream HTTP status:

- **200** (and any other 2xx/3xx, and real 4xx) → stream straight back, unbuffered (SSE intact).
- **429** → honor `Retry-After`, then exponential backoff with jitter, and retry.
- **500–599** → same backoff/retry (no `Retry-After`).
- **connect error** → treated like 5xx (backoff + retry).

> Anthropic sometimes returns an error *inside* a 200 SSE stream (an `event: error` carrying
> `overloaded_error` / `rate_limit_error`) instead of as an HTTP status. The status-only dispatch
> above passes those straight through; `ANTHROXY_DEBUG` logs just the error frame so they can be
> observed. Treating them as retryable is a likely next step.

Retries continue until a **per-request** wall-clock deadline (`Instant::now() +
ANTHROXY_MAX_RETRY_WINDOW_S`, stamped fresh for each request — no shared timer, so concurrent
requests never contend or starve each other). If the window is exhausted (a real quota exhaustion,
a long outage), the last upstream response is passed through as-is.

There is **no shared mutable state** — no concurrency semaphore, no start-pacing mutex. That makes
anthroxy stateless and trivially safe under high concurrency; the request body is buffered
per-request only so a retry can re-send it.

Built in Rust (tokio + hyper + reqwest/rustls). Headers are forwarded verbatim (only hop-by-hop
`host`/`connection`/`content-length` are dropped); TLS roots are bundled via rustls, so the runtime
image needs no system CA bundle.

## Configuration

All via environment variables:

| Variable                      | Default                     | Purpose                                              |
| ----------------------------- | --------------------------- | ---------------------------------------------------- |
| `ANTHROXY_BIND`               | `127.0.0.1`                 | Listen address (loopback by default — see below)     |
| `ANTHROXY_PORT`               | `8787`                      | Listen port                                          |
| `ANTHROXY_UPSTREAM`           | `https://api.anthropic.com` | Upstream base URL                                    |
| `ANTHROXY_MAX_RETRY_WINDOW_S` | `600`                       | Wall-clock budget to keep retrying 429/5xx (per req) |
| `ANTHROXY_BASE_BACKOFF_MS`    | `500`                       | Base for exponential backoff between retries         |
| `ANTHROXY_DEBUG`              | `off`                       | Log upstream error frames only (incl. errors inside a 200 SSE stream); never logs request bodies or content |

The bind defaults to loopback on purpose: anthroxy is an open relay to Anthropic for whoever can
reach it, so it should never listen on a wildcard. Front it with a reverse proxy (e.g. Caddy) if
you want it reachable under a hostname, and keep the proxy itself on `127.0.0.1`.

## Usage

```sh
anthroxy &
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude -p "hello"
```

For Claude Code fleet-wide, set it in `settings.json` (there is no dedicated key — the `env` block
is the canonical place):

```json
{ "env": { "ANTHROPIC_BASE_URL": "https://anthroxy.your.domain" } }
```

## Deploy

The `Dockerfile` produces a distroless image (~29 MB). `anthroxy.nomad.hcl` runs it as a Nomad/
podman service bound to loopback, fronted by Caddy. `count = 1` is operational simplicity, not a
correctness requirement — anthroxy is stateless and could scale horizontally if ever needed.

## What we measured

The "react, don't anticipate" design is grounded, not assumed. Probing with the official
`claude -p` CLI (Opus, ground-truth statuses read off a transparent observe-mode instance):

- Single concurrent bursts up to **100 parallel agents (~200 in-flight requests)** → zero API
  `429`s. Request-concurrency is not the limiter (consistent with Anthropic's own Deep Research
  spawning 100+ parallel subagents).
- Sustained start-rates (2/s, ~4/s over 30–60s) → zero `429`s. Onset rate isn't it either.
- The only throttle we ever reproduced was an artifact of *stacking* experiments without a
  cooldown — it vanished when each run started clean.

Caveat: this measured *request-concurrency* with trivial payloads. The *token-throughput* axis
(ITPM/OTPM) is untested — but a token-rate limit isn't something a concurrency cap or start-pacing
would help with anyway. anthroxy's job is to survive whatever 429 shows up, not to predict it.
