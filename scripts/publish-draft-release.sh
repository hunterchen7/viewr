#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/publish-draft-release.sh --release-id ID --release-sha SHA <release-tag>" >&2
}

fail() {
  echo "error: $*" >&2
  exit 1
}

release_id=""
release_sha=""
while [[ $# -ge 2 ]]; do
  case "$1" in
    --release-id)
      release_id="$2"
      shift 2
      ;;
    --release-sha)
      release_sha="$2"
      shift 2
      ;;
    *)
      break
      ;;
  esac
done
if [[ $# -ne 1 ]]; then
  usage
  exit 2
fi

release_tag="$1"
repository="${GITHUB_REPOSITORY:-}"
query_attempts="${VIEWR_RELEASE_QUERY_ATTEMPTS:-6}"
query_delay_seconds="${VIEWR_RELEASE_QUERY_DELAY_SECONDS:-2}"

[[ "$release_id" =~ ^[1-9][0-9]*$ ]] ||
  fail "release ID must be a positive integer"
[[ "$release_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] ||
  fail "release SHA must be a full lowercase Git object ID"
[[ "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail "release tag is not a stable semantic version: $release_tag"
[[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  fail "GITHUB_REPOSITORY must identify one owner and repository"
[[ -n "${GH_TOKEN:-}" ]] || fail "GH_TOKEN is not set"
[[ "$query_attempts" =~ ^[1-9][0-9]*$ ]] ||
  fail "VIEWR_RELEASE_QUERY_ATTEMPTS must be a positive integer"
[[ "$query_delay_seconds" =~ ^[0-9]+$ ]] ||
  fail "VIEWR_RELEASE_QUERY_DELAY_SECONDS must be a non-negative integer"
command -v gh >/dev/null || fail "gh is required"
command -v jq >/dev/null || fail "jq is required"

matches_state() {
  local expected_draft="$1"
  jq -e \
    --arg tag "$release_tag" \
    --argjson id "$release_id" \
    --argjson expected_draft "$expected_draft" \
    '
      .id == $id
      and .tag_name == $tag
      and .draft == $expected_draft
      and .prerelease == false
    ' >/dev/null
}

release_json="$(
  gh api "repos/$repository/releases/$release_id"
)"
if ! matches_state true <<<"$release_json"; then
  fail "GitHub did not return the expected stable draft release"
fi

tag_object="$(
  gh api "repos/$repository/git/ref/tags/$release_tag"
)"
object_type="$(jq -er '.object.type | strings' <<<"$tag_object")"
object_sha="$(jq -er '.object.sha | strings' <<<"$tag_object")"
tag_depth=0
while [[ "$object_type" == "tag" && "$tag_depth" -lt 8 ]]; do
  [[ "$object_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] ||
    fail "release tag contains an invalid object ID"
  tag_object="$(
    gh api "repos/$repository/git/tags/$object_sha"
  )"
  object_type="$(jq -er '.object.type | strings' <<<"$tag_object")"
  object_sha="$(jq -er '.object.sha | strings' <<<"$tag_object")"
  tag_depth=$((tag_depth + 1))
done
[[ "$object_type" == "commit" ]] ||
  fail "release tag does not resolve to a commit"
[[ "$object_sha" == "$release_sha" ]] ||
  fail "release tag moved from $release_sha to $object_sha"

publish_response_valid=false
if publish_json="$(
  gh api \
    --method PATCH \
    "repos/$repository/releases/$release_id" \
    -F draft=false
)"; then
  if matches_state false <<<"$publish_json"; then
    publish_response_valid=true
  else
    echo "::warning::GitHub returned an unexpected publication response; checking the exact release state." >&2
  fi
else
  echo "::warning::The publication request returned an error; checking whether it took effect." >&2
fi

attempt=1
while ((attempt <= query_attempts)); do
  release_json=""
  if release_json="$(
    gh api "repos/$repository/releases/$release_id"
  )" && matches_state false <<<"$release_json"; then
    echo "Published $repository release $release_tag (ID $release_id)"
    exit 0
  fi

  if ((attempt == query_attempts)); then
    break
  fi
  echo "::warning::Release publication is not visible yet (attempt $attempt/$query_attempts); retrying." >&2
  sleep "$query_delay_seconds"
  attempt=$((attempt + 1))
done

if [[ "$publish_response_valid" == "true" ]]; then
  fail "release $release_id was published but its final state could not be verified"
fi
fail "publication of release $release_id could not be confirmed"
