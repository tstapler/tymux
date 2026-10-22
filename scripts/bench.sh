#!/usr/bin/env bash
# Build release tymuxd, run the tymux-vs-tmux benchmark, and optionally render
# charts. Paths are relative to the repo root.
# Usage: scripts/bench.sh [--quick] [--out docs/benchmarks/results.json]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p tymuxd
bin="$(mktemp -d)/bench"
(cd bench && go build -o "$bin" .)
"$bin" "$@"
rm -rf "$(dirname "$bin")"
