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
        host_network = "service"
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
        # Bind ONLY the "service" host_network IP (192.168.88.3), never 0.0.0.0 — anthroxy's
        # configurable bind does what plane-api's gunicorn TODO wanted but couldn't easily.
        ANTHROXY_BIND = "${NOMAD_IP_http}"
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
