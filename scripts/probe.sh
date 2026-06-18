#!/usr/bin/env bash
# anthroxy rate-limit probe — reproduces how we measured Anthropic's throttle behavior, and
# stands as runnable documentation of it.
#
# METHODOLOGY (why it's built this way — these are hard-won, not arbitrary):
#  - Load is generated with the official `claude -p` CLI: real workload, real OAuth creds, no
#    protocol replication.
#  - `claude -p --debug` emits NO HTTP signal in print mode, and the CLI retries some failures
#    internally — so the client alone cannot tell you the raw upstream status. Instead we route
#    it through a TRANSPARENT observe-mode anthroxy (retry window = 0 => pure pass-through that
#    logs every response) and read GROUND-TRUTH statuses from that tap's log. Every log line is
#    one real Anthropic response.
#  - The endpoint is forced per-invocation with `--settings`, because a plain exported
#    ANTHROPIC_BASE_URL is overridden by ~/.claude/settings.json.
#  - Each `claude -p` makes >1 HTTP request (a HEAD probe + the message), so tap line counts
#    exceed the launch count; reason about the tap totals, not the launch count.
#
# WHAT WE OBSERVED (2026-06-18, Opus): request-concurrency is NOT the limiter. Single bursts up
# to ~100 parallel agents (~200 in-flight) and sustained 2-4/s both returned ZERO 429s. The only
# 429s we ever produced came from stacking runs with no cooldown between them. The token-throughput
# axis (ITPM/OTPM) was NOT tested — to probe it, raise PROBE_PROMPT to force large input/output.
#
# USAGE:
#   probe.sh burst <count>                # N parallel one-shot starts        (concurrency axis)
#   probe.sh rate  <interval_ms> <secs>   # a start every <interval_ms> for <secs>  (onset-rate axis)
#
# ENV: PROBE_MODEL (default claude-opus-4-8)   PROBE_PROMPT (default a one-token reply)
#      PROBE_PORT  (default 8789)              PROBE_IMAGE  (default localhost/anthroxy:0.2.0)
#      PROBE_COOLDOWN_S (default 25)  trailing drain so your NEXT cli call isn't throttled by the tail
set -u

PROBE_MODEL="${PROBE_MODEL:-claude-opus-4-8}"
PROBE_PROMPT="${PROBE_PROMPT:-Reply with the single token: ok}"
PROBE_PORT="${PROBE_PORT:-8789}"
PROBE_IMAGE="${PROBE_IMAGE:-localhost/anthroxy:0.2.0}"
PROBE_COOLDOWN_S="${PROBE_COOLDOWN_S:-25}"
TAP_NAME="anthroxy-probe-tap"
TAP_URL="http://127.0.0.1:${PROBE_PORT}"
SETTINGS="{\"env\":{\"ANTHROPIC_BASE_URL\":\"${TAP_URL}\"}}"

usage() {
  cat <<'U'
anthroxy rate-limit probe (tap-based, ground-truth)
  probe.sh burst <count>                N parallel one-shot starts        (concurrency axis)
  probe.sh rate  <interval_ms> <secs>   sustained start cadence           (onset-rate axis)
env: PROBE_MODEL PROBE_PROMPT PROBE_PORT PROBE_IMAGE PROBE_COOLDOWN_S
U
  exit 1
}

MODE="${1:-}"; shift || true
[ -z "$MODE" ] && usage

# Transparent observe tap: retry window 0 => never retries, just forwards and logs the raw status.
cleanup() { podman rm -f "$TAP_NAME" >/dev/null 2>&1; }
trap cleanup EXIT
cleanup
podman run -d --rm --name "$TAP_NAME" --network host \
  -e ANTHROXY_BIND=127.0.0.1 -e ANTHROXY_PORT="$PROBE_PORT" -e ANTHROXY_MAX_RETRY_WINDOW_S=0 \
  "$PROBE_IMAGE" >/dev/null 2>&1 || { echo "could not start tap from image '$PROBE_IMAGE'"; exit 1; }
sleep 2

ms() { date +%s%3N; }
wd="$(mktemp -d /tmp/anthroxy-probe.XXXXXX)"; : >"$wd/exits"
launch() { ( claude -p "$PROBE_PROMPT" --model "$PROBE_MODEL" --settings "$SETTINGS" >/dev/null 2>&1
             echo $? >>"$wd/exits" ) & }

case "$MODE" in
  burst)
    N="${1:?usage: probe.sh burst <count>}"
    echo "burst: $N parallel $PROBE_MODEL starts -> $TAP_URL"
    for _ in $(seq 1 "$N"); do launch; done
    wait
    ;;
  rate)
    IV="${1:?usage: probe.sh rate <interval_ms> <secs>}"; SECS="${2:?usage: probe.sh rate <interval_ms> <secs>}"
    echo "rate: one $PROBE_MODEL start every ${IV}ms for ${SECS}s -> $TAP_URL"
    deadline=$(( $(ms) + SECS * 1000 ))
    while [ "$(ms)" -lt "$deadline" ]; do launch; sleep "$(awk "BEGIN{printf \"%.3f\", $IV/1000}")"; done
    wait
    ;;
  *) usage ;;
esac

sleep 4 # let the tap's log settle
log="$(podman logs "$TAP_NAME" 2>&1)"
n429=$(printf '%s\n' "$log" | grep -cE '> 429 ')
nfail=$(awk '$1 != 0' "$wd/exits" 2>/dev/null | wc -l | tr -d ' ')

echo
echo "=== tap ground-truth statuses (each line = one real Anthropic response) ==="
printf '%s\n' "$log" | grep -oE '> [0-9]{3} ' | sort | uniq -c
echo "=== claude -p exit codes ==="
sort "$wd/exits" | uniq -c
echo
echo "INTERPRETATION"
echo "  API throttle : $n429   (tap 429s = real Anthropic rate limiting)"
echo "  client fails : $nfail   (claude exit!=0 WITHOUT a matching tap-429 => LOCAL resource limits, not Anthropic)"
[ "$n429" -gt 0 ] && { echo "  --- 429 lines ---"; printf '%s\n' "$log" | grep -E '> 429 ' | head -10; }
echo
echo "cooldown ${PROBE_COOLDOWN_S}s (drain the burst tail so your next CLI call isn't throttled)..."
sleep "$PROBE_COOLDOWN_S"
echo "done (tap removed; raw launch exits in $wd)"
