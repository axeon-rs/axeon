#!/usr/bin/env bash
# Compares Axeon with axum, actix-web and Express on three endpoints.
# Requires: ab (ApacheBench), node/npm, a Rust toolchain.
# Usage: benchmarks/run.sh [requests-per-client]
set -euo pipefail
cd "$(dirname "$0")"
N=${1:-50000}      # requests per ab client
CLIENTS=4          # parallel ab processes (one ab is single-threaded)
CONC=32            # connections per ab process

(cd rust && cargo build --release --quiet)
(cd express && npm install --silent --no-audit --no-fund >/dev/null)

run_server() { # name port command...
  local name=$1 port=$2; shift 2
  "$@" >/dev/null 2>&1 &
  local pid=$!
  for _ in $(seq 50); do curl -sf "http://127.0.0.1:$port/plaintext" >/dev/null && break; sleep 0.1; done
  for path in /plaintext /json /users/42; do
    ab -q -k -c $CONC -n 5000 "http://127.0.0.1:$port$path" >/dev/null 2>&1   # warm-up
    local out; out=$(mktemp)
    for _ in $(seq $CLIENTS); do ab -q -k -c $CONC -n "$N" "http://127.0.0.1:$port$path" >>"$out" 2>&1 & done
    wait $(jobs -p | grep -v "^$pid$") 2>/dev/null || true
    local rps p99 failed
    rps=$(awk '/Requests per second/ {s += $4} END {printf "%.0f", s}' "$out")
    p99=$(awk '/^  99%/ {if ($2 > m) m = $2} END {print m}' "$out")
    failed=$(awk '/Failed requests/ {s += $3} END {print s}' "$out")
    printf "%-8s %-11s %10s req/s   p99 %4s ms   failed %s\n" "$name" "$path" "$rps" "$p99" "$failed"
    rm -f "$out"
  done
  # Resident memory of the server (and its workers) after the load.
  local rss; rss=$(ps -o rss= -p "$pid" $(pgrep -P "$pid" || true) | awk '{s += $1} END {printf "%.1f", s / 1024}')
  printf "%-8s memory after load: %s MiB\n\n" "$name" "$rss"
  kill "$pid" $(pgrep -P "$pid" || true) 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  sleep 1
}

echo "CPU: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), $(getconf _NPROCESSORS_ONLN) cores"
echo "Load: $CLIENTS x ab -k -c $CONC -n $N per endpoint (client and server on the same machine)"
echo
run_server axeon   8001 rust/target/release/axeon
run_server axum    8002 rust/target/release/axum
run_server actix   8003 rust/target/release/actix
run_server express 8004 node express/server.js
