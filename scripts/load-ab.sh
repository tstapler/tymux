#!/usr/bin/env bash
# A/B the latency probe (crates/tymux-e2e/tests/load_latency_e2e.rs) across tymuxd binaries.
#
#   scripts/load-ab.sh [-r ROUNDS] label=/path/to/tymuxd [label=/path/to/tymuxd ...]
#
# Each round runs every binary once, back to back, alternating the order between rounds so
# neither side always gets the quieter half of a noisy host. Each run uses a fresh isolated
# daemon (own port, socket and state dir), so the live daemon is never touched. Prints the
# median across rounds of each percentile, plus the per-round host load average.
# Pinned parameters come from the probe's LOAD_* defaults; override them in the environment.
set -euo pipefail

rounds=5
if [[ "${1:-}" == "-r" ]]; then rounds="$2"; shift 2; fi
(($# >= 1)) || { echo "usage: $0 [-r ROUNDS] label=path ..." >&2; exit 2; }

cd "$(dirname "$0")/.."
cargo test --release -p tymux-e2e --test load_latency_e2e --no-run >&2
test_bin="$(ls -t target/release/deps/load_latency_e2e-* | grep -v '\.d$' | head -1)"
out="$(mktemp -t load-ab.XXXXXX)"
specs=("$@")

for ((round = 1; round <= rounds; round++)); do
  order=("${specs[@]}")
  if ((round % 2 == 0)); then
    order=()
    for ((i = ${#specs[@]} - 1; i >= 0; i--)); do order+=("${specs[i]}"); done
  fi
  echo "round $round load: $(uptime | sed 's/.*load averages*: //')" >&2
  for spec in "${order[@]}"; do
    LOAD_BIN="${spec#*=}" LOAD_LABEL="${spec%%=*}" "$test_bin" --ignored --nocapture 2>/dev/null |
      grep '^RESULT' | sed "s/^RESULT /RESULT round=$round /" >>"$out"
  done
done

echo "raw results: $out" >&2
# Median across rounds of p50/p95/p99 per (bin, noisy, metric).
awk '
  /metric=echo_lost/ { for (i = 1; i <= NF; i++) if ($i ~ /^(bin|noisy|storm|n)=/) { split($i, kv, "="); f[kv[1]] = kv[2] }
                       lost[f["bin"] " noisy=" f["noisy"]] += f["n"]; next }
  { for (i = 1; i <= NF; i++) { split($i, kv, "="); f[kv[1]] = kv[2] }
    key = f["bin"] "\t" f["noisy"] "\t" f["metric"]
    for (q = 1; q <= 3; q++) { name = (q == 1 ? "p50" : q == 2 ? "p95" : "p99"); vals[key, name] = vals[key, name] " " f[name] }
    keys[key] = 1 }
  function median(s,   a, n, i, j, t) {
    n = split(s, a, " "); for (i = 1; i <= n; i++) for (j = i + 1; j <= n; j++) if (a[j] + 0 < a[i] + 0) { t = a[i]; a[i] = a[j]; a[j] = t }
    return (n % 2) ? a[(n + 1) / 2] : (a[n / 2] + a[n / 2 + 1]) / 2 }
  END { printf "%-14s %6s %-14s %8s %8s %8s\n", "bin", "noisy", "metric", "p50ms", "p95ms", "p99ms"
        for (k in keys) { split(k, p, "\t"); printf "%-14s %6s %-14s %8.2f %8.2f %8.2f\n", p[1], p[2], p[3], median(vals[k, "p50"]), median(vals[k, "p95"]), median(vals[k, "p99"]) | "sort -k2,2n -k3,3 -k1,1"
        }
        close("sort -k2,2n -k3,3 -k1,1")
        for (k in lost) printf "echo_lost total %s: %d\n", k, lost[k] }
' "$out"
