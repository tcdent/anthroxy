job "anthroxy" {
  datacenters = ["dc1"]
  type        = "service"

  group "proxy" {
    # anthroxy is stateless now (per-request retry timer, no shared counters), so it could scale
    # horizontally — but one instance is plenty for the fleet (async; ~200 concurrent is trivial).
    count = 1

    network {
      mode = "host"

      port "http" {
        static       = 8787
        host_network = "default"
      }
    }

    task "anthroxy" {
      driver = "podman"

      config {
        image        = "localhost/anthroxy:0.2.1"
        ports        = ["http"]
        network_mode = "host"
      }

      env {
        # Loopback-only: Caddy (network_mode=host, same netns) is anthroxy's sole ingress at
        # https://anthroxy.a10k.co. Nothing reaches the proxy directly — no wildcard, no host IP.
        ANTHROXY_BIND = "127.0.0.1"
        ANTHROXY_PORT = "${NOMAD_PORT_http}"

        # React, don't anticipate: no concurrency cap or start-pacing — measurement (INFRA-32)
        # showed request-concurrency isn't the limit (100+ parallel agents run fine). Keep retrying
        # 429/5xx (honoring Retry-After) for up to 10 min so an agent never halts on a transient
        # throttle or blip.
        ANTHROXY_MAX_RETRY_WINDOW_S = "600"
        ANTHROXY_BASE_BACKOFF_MS    = "500"

        # Log upstream error frames only (incl. errors that arrive inside a 200 SSE stream) — cheap,
        # non-spammy observability that stays silent until something actually errors. "0" disables.
        ANTHROXY_DEBUG = "1"
      }

      resources {
        cpu    = 100
        memory = 64
      }
    }
  }
}
