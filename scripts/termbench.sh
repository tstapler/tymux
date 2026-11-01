#!/usr/bin/env bash
# Build release tymuxd and run the browser-terminal latency benchmark
# (xterm.js, xterm.js+WebGL, ghostty-web in headless Chrome). Paths are
# relative to the repo root. Needs Go, npm and Chrome/Chromium.
# Usage: scripts/termbench.sh [--quick] [--out FILE] [any termbench flag]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p tymuxd
(cd bench/termbench/web && npm ci --no-audit --no-fund)
bin_dir="$(mktemp -d)"
trap 'rm -rf "$bin_dir"' EXIT
bin="$bin_dir/termbench"
(cd bench && go build -o "$bin" ./termbench)
"$bin" "$@"
