#!/usr/bin/env bash
# Build release tymuxd and run the tymux-vs-tmux benchmark.
# Usage: scripts/bench.sh [--quick] [--out docs/benchmarks/results.json]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p tymuxd
cd bench
exec go run . --tymuxd ../target/release/tymuxd "$@"
