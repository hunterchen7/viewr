#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -ge 1 && $# -le 2 ]] ||
  fail "usage: scripts/require-draft-release.sh vMAJOR.MINOR.PATCH [EXPECTED-RELEASE-ID]"

release_tag="$1"
expected_release_id="${2:-}"
repository="${GITHUB_REPOSITORY:-}"
query_attempts="${VIEWR_RELEASE_QUERY_ATTEMPTS:-18}"
query_delay_seconds="${VIEWR_RELEASE_QUERY_DELAY_SECONDS:-10}"

[[ "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail "release tag is not a stable semantic version: $release_tag"
[[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  fail "GITHUB_REPOSITORY must identify one owner and repository"
[[ "$query_attempts" =~ ^[1-9][0-9]*$ ]] ||
  fail "VIEWR_RELEASE_QUERY_ATTEMPTS must be a positive integer"
[[ "$query_delay_seconds" =~ ^[0-9]+$ ]] ||
  fail "VIEWR_RELEASE_QUERY_DELAY_SECONDS must be a non-negative integer"
if [[ -n "$expected_release_id" && ! "$expected_release_id" =~ ^[1-9][0-9]*$ ]]; then
  fail "expected release ID must be a positive integer"
fi
command -v gh >/dev/null || fail "gh is required"
command -v jq >/dev/null || fail "jq is required"

query_error_file="$(mktemp "${TMPDIR:-/tmp}/viewr-release-query.XXXXXX")"
trap 'rm -f "$query_error_file"' EXIT

attempt=1
while ((attempt <= query_attempts)); do
  release_pages=""
  query_error=""
  : >"$query_error_file"
  if release_pages="$(
    gh api \
      --paginate \
      "repos/$repository/releases?per_page=100" 2>"$query_error_file"
  )"; then
    if [[ -s "$query_error_file" ]]; then
      cat "$query_error_file" >&2
    fi
    if ! matching_releases="$(
      jq -sc \
        --arg tag "$release_tag" \
        '[.[][] | select(.tag_name == $tag)]' \
        <<<"$release_pages"
    )"; then
      echo "::error::GitHub returned invalid release data." >&2
      exit 1
    fi

    match_count="$(jq -r 'length' <<<"$matching_releases")"
    case "$match_count" in
      0)
        query_error="release is not present in the authenticated release list"
        ;;
      1)
        if ! release_id="$(
          jq -er \
            '
              .[0]
              | select(.draft == true and .prerelease == false)
              | .id
              | numbers
            ' \
            <<<"$matching_releases"
        )"; then
          echo "::error::Release $release_tag exists but is not a stable draft." >&2
          exit 1
        fi
        if [[ ! "$release_id" =~ ^[1-9][0-9]*$ ]]; then
          echo "::error::Release $release_tag has an invalid numeric ID." >&2
          exit 1
        fi
        if [[ -n "$expected_release_id" && "$release_id" != "$expected_release_id" ]]; then
          echo "::error::Release $release_tag changed from ID $expected_release_id to $release_id." >&2
          exit 1
        fi
        printf '%s\n' "$release_id"
        exit 0
        ;;
      *)
        echo "::error::GitHub returned $match_count releases for tag $release_tag." >&2
        exit 1
        ;;
    esac
  else
    query_error="$(<"$query_error_file")"
  fi

  if ((attempt == query_attempts)); then
    echo "::error::Could not resolve stable draft release $release_tag after $query_attempts attempts." >&2
    echo "$query_error" >&2
    exit 1
  fi

  echo "::warning::Could not resolve stable draft release $release_tag (attempt $attempt/$query_attempts); retrying." >&2
  sleep "$query_delay_seconds"
  attempt=$((attempt + 1))
done

fail "release visibility check ended unexpectedly"
