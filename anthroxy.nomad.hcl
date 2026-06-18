job "anthroxy" {
  datacenters = ["dc1"]
  type        = "service"

  group "proxy" {
    # The rate-limit state (concurrency cap + start pacing) lives in-memory, so this
    # must be EXACTLY one instance — a second would split the budget and double the
    # effective concurrency against Anthropic.
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
        image        = "localhost/anthroxy:0.1.0"
        ports        = ["http"]
        network_mode = "host"
      }

      env {
        # Loopback-only: Caddy (network_mode=host, same netns) is anthroxy's sole ingress at
        # https://anthroxy.a10k.co. Nothing reaches the proxy directly — no wildcard, no host IP.
        ANTHROXY_BIND = "127.0.0.1"
        ANTHROXY_PORT = "${NOMAD_PORT_http}"

        # Throttle knobs — start conservative (~the 3-concurrent acceleration ceiling we
        # observed) and loosen against live traffic.
        ANTHROXY_MAX_CONCURRENCY = "3"
        ANTHROXY_MIN_INTERVAL_MS = "350"
        ANTHROXY_MAX_RETRIES     = "8"
        ANTHROXY_BASE_BACKOFF_MS = "500"
      }

      resources {
        cpu    = 100
        memory = 64
      }
    }
  }
}
