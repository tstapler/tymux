#!/usr/bin/env bash
# Pulls upstream taiki-e commits into our forks of the release actions and
# prints what changed since the SHA release.yml pins, so the pin bump can be
# reviewed. It does not edit release.yml: bump the SHA in a PR after reading
# the log (these actions run with contents: write).
#
# If a fork has diverged (we have local commits), `gh repo sync` refuses to
# fast-forward; merge upstream into the fork's main by hand instead:
#   git clone git@github.com:tstapler/<action>.git && cd <action> &&
#   git remote add upstream https://github.com/taiki-e/<action>.git &&
#   git fetch upstream && git merge upstream/main
set -euo pipefail

workflow="$(dirname "$0")/../.github/workflows/release.yml"
owner=tstapler

for action in create-gh-release-action upload-rust-binary-action; do
  pinned=$(grep -oE "$owner/$action@[0-9a-f]{40}" "$workflow" | head -1 | cut -d@ -f2)
  gh repo sync "$owner/$action" --source "taiki-e/$action" --branch main
  head=$(gh api "repos/$owner/$action/commits/main" --jq .sha)
  echo "== $action: pinned ${pinned:0:12}, fork main ${head:0:12}"
  if [ "$pinned" = "$head" ]; then
    echo "   up to date"
  else
    gh api "repos/$owner/$action/compare/$pinned...$head" \
      --jq '.commits[] | "   \(.sha[0:8]) \(.commit.message | split("\n")[0])"'
  fi
done
