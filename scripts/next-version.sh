#!/usr/bin/env bash
# Prints the next release version (no "v" prefix) for the commits on HEAD since
# the latest v* tag, or nothing if there are none. Used by
# .github/workflows/auto-release.yml; run it locally to preview.
#
# Bump level comes from Conventional Commit subjects (squash-merged PR titles):
#   type!: ... or a "BREAKING CHANGE" footer -> major
#   feat: / feat(scope): ...                 -> minor
#   anything else (fix, perf, chore, ...)    -> patch
set -euo pipefail

last_tag=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
if [ -z "$last_tag" ]; then
  echo "no v* tag reachable from HEAD" >&2
  exit 1
fi

range="$last_tag..HEAD"
[ -n "$(git rev-list "$range" --max-count=1)" ] || exit 0

level=patch
if git log --format=%B "$range" | grep -Eq '^BREAKING[ -]CHANGE'; then
  level=major
elif git log --format=%s "$range" | grep -Eq '^[a-z]+(\([^)]*\))?!:'; then
  level=major
elif git log --format=%s "$range" | grep -Eq '^feat(\([^)]*\))?:'; then
  level=minor
fi

IFS=. read -r major minor patch <<<"${last_tag#v}"
case "$level" in
  major) echo "$((major + 1)).0.0" ;;
  minor) echo "$major.$((minor + 1)).0" ;;
  patch) echo "$major.$minor.$((patch + 1))" ;;
esac
