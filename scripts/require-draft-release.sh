#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -eq 1 ]] || fail "usage: scripts/require-draft-release.sh vMAJOR.MINOR.PATCH"

release_tag="$1"
repository="${GITHUB_REPOSITORY:-}"
query_attempts="${VIEWR_RELEASE_QUERY_ATTEMPTS:-18}"
query_delay_seconds="${VIEWR_RELEASE_QUERY_DELAY_SECONDS:-10}"

[[ "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail "release tag is not a stable semantic version: $release_tag"
[[ -n "$repository" ]] || fail "GITHUB_REPOSITORY is not set"
[[ "$query_attempts" =~ ^[1-9][0-9]*$ ]] ||
  fail "VIEWR_RELEASE_QUERY_ATTEMPTS must be a positive integer"
[[ "$query_delay_seconds" =~ ^[0-9]+$ ]] ||
  fail "VIEWR_RELEASE_QUERY_DELAY_SECONDS must be a non-negative integer"
command -v gh >/dev/null || fail "gh is required"
command -v jq >/dev/null || fail "jq is required"

attempt=1
while ((attempt <= query_attempts)); do
  release_json=""
  if release_json="$(
    gh release view \
      "$release_tag" \
      --repo "$repository" \
      --json isDraft,tagName 2>&1
  )"; then
    if jq -e \
      --arg tag "$release_tag" \
      '.tagName == $tag and .isDraft == true' \
      <<<"$release_json" >/dev/null; then
      exit 0
    fi

    echo "::error::Release $release_tag exists but is not the expected draft." >&2
    exit 1
  fi

  if ((attempt == query_attempts)); then
    echo "::error::Release $release_tag did not become visible after $query_attempts attempts." >&2
    echo "$release_json" >&2
    exit 1
  fi

  echo "::warning::Release $release_tag is not visible yet (attempt $attempt/$query_attempts); retrying." >&2
  sleep "$query_delay_seconds"
  attempt=$((attempt + 1))
done

fail "release visibility check ended unexpectedly"
