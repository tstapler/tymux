#!/usr/bin/env bash
# Print the Homebrew formula for a tymux release to stdout.
#   usage: render-homebrew-formula.sh VERSION DIR
#   VERSION  release version without the leading v (e.g. 1.17.4)
#   DIR      directory holding the release's tymux-<target>.tar.gz archives; their sha256 sums
#            are computed here so the formula never trusts a separately published checksum.
set -euo pipefail

version=${1:?usage: render-homebrew-formula.sh VERSION DIR}
dir=${2:?usage: render-homebrew-formula.sh VERSION DIR}
base="https://github.com/tstapler/tymux/releases/download/v${version}"

sha() {
  local f="$dir/tymux-$1.tar.gz"
  [ -f "$f" ] || { echo "render-homebrew-formula: missing $f" >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$f"; else shasum -a 256 "$f"; fi | cut -d' ' -f1
}

mac_arm=$(sha aarch64-apple-darwin)
mac_x86=$(sha x86_64-apple-darwin)
lin_arm=$(sha aarch64-unknown-linux-musl)
lin_x86=$(sha x86_64-unknown-linux-musl)

cat <<FORMULA
class Tymux < Formula
  desc "Terminal multiplexer daemon (tymuxd) and CLI (tymux)"
  homepage "https://github.com/tstapler/tymux"
  version "${version}"
  license "MIT"

  on_macos do
    on_arm do
      url "${base}/tymux-aarch64-apple-darwin.tar.gz"
      sha256 "${mac_arm}"
    end
    on_intel do
      url "${base}/tymux-x86_64-apple-darwin.tar.gz"
      sha256 "${mac_x86}"
    end
  end

  on_linux do
    on_arm do
      url "${base}/tymux-aarch64-unknown-linux-musl.tar.gz"
      sha256 "${lin_arm}"
    end
    on_intel do
      url "${base}/tymux-x86_64-unknown-linux-musl.tar.gz"
      sha256 "${lin_x86}"
    end
  end

  def install
    bin.install "tymuxd", "tymux"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tymuxd --version")
  end
end
FORMULA
