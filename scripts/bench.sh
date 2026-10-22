#!/usr/bin/env bash
# Build release binaries and run the tymux-vs-tmux benchmark. Paths are
# relative to the repo root.
# Usage: scripts/bench.sh [--quick] [--out FILE]
# Then regenerate charts: cargo run -p tymux-bench -- render
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p tymuxd -p tymux-cli
bin="$(mktemp -d)/bench"
(cd bench && go build -o "$bin" .)
"$bin" "$@"
rm -rf "$(dirname "$bin")"
