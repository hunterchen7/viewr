#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/publish-draft-release.sh --asset-directory DIR --release-id ID --release-sha SHA [--recovery-workflow-sha SHA] <release-tag>" >&2
}

fail() {
  echo "error: $*" >&2
  exit 1
}

release_id=""
release_sha=""
recovery_workflow_sha=""
asset_directory=""
while [[ $# -ge 2 ]]; do
  case "$1" in
    --asset-directory)
      asset_directory="$2"
      shift 2
      ;;
    --release-id)
      release_id="$2"
      shift 2
      ;;
    --release-sha)
      release_sha="$2"
      shift 2
      ;;
    --recovery-workflow-sha)
      recovery_workflow_sha="$2"
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
query_attempts="${VIEWR_RELEASE_QUERY_ATTEMPTS:-18}"
query_delay_seconds="${VIEWR_RELEASE_QUERY_DELAY_SECONDS:-10}"

[[ "$release_id" =~ ^[1-9][0-9]*$ ]] ||
  fail "release ID must be a positive integer"
[[ "$release_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] ||
  fail "release SHA must be a full lowercase Git object ID"
if [[ -n "$recovery_workflow_sha" ]] &&
  [[ ! "$recovery_workflow_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]]; then
  fail "recovery workflow SHA must be a full lowercase Git object ID"
fi
[[ "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail "release tag is not a stable semantic version: $release_tag"
[[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  fail "GITHUB_REPOSITORY must identify one owner and repository"
[[ -n "${GH_TOKEN:-}" ]] || fail "GH_TOKEN is not set"
[[ -d "$asset_directory" ]] ||
  fail "asset directory does not exist: $asset_directory"
[[ "$query_attempts" =~ ^[1-9][0-9]*$ ]] ||
  fail "VIEWR_RELEASE_QUERY_ATTEMPTS must be a positive integer"
[[ "$query_delay_seconds" =~ ^[0-9]+$ ]] ||
  fail "VIEWR_RELEASE_QUERY_DELAY_SECONDS must be a non-negative integer"
command -v gh >/dev/null || fail "gh is required"
command -v jq >/dev/null || fail "jq is required"

version="${release_tag#v}"
expected_assets=(
  "SHA256SUMS"
  "viewr-$version-source.tar.gz"
  "viewr-linux-x64.deb"
  "viewr-linux-x64.tar.gz"
  "viewr-macos-arm64.pkg"
  "viewr-macos-arm64.tar.gz"
  "viewr-windows-x64.msi"
  "viewr-windows-x64.zip"
)
invalid_entry="$(
  find "$asset_directory" \
    -mindepth 1 \
    -maxdepth 1 \
    ! -type f \
    -print \
    -quit
)"
[[ -z "$invalid_entry" ]] ||
  fail "asset directory contains a non-regular entry: $invalid_entry"
actual_assets="$(
  find "$asset_directory" \
    -mindepth 1 \
    -maxdepth 1 \
    -type f \
    -exec basename {} \; |
    LC_ALL=C sort
)"
sorted_expected_assets="$(
  printf '%s\n' "${expected_assets[@]}" |
    LC_ALL=C sort
)"
[[ "$actual_assets" == "$sorted_expected_assets" ]] ||
  fail "asset directory does not contain the exact release artifact set"
while IFS= read -r asset_name; do
  [[ -s "$asset_directory/$asset_name" ]] ||
    fail "asset is empty: $asset_name"
done <<<"$actual_assets"

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

resolve_tag_commit() {
  local tag_depth=0
  local tag_object
  local object_type
  local object_sha

  tag_object="$(
    gh api "repos/$repository/git/ref/tags/$release_tag"
  )" || return 1
  object_type="$(jq -er '.object.type | strings' <<<"$tag_object")" ||
    return 1
  object_sha="$(jq -er '.object.sha | strings' <<<"$tag_object")" ||
    return 1

  while [[ "$object_type" == "tag" && "$tag_depth" -lt 8 ]]; do
    if [[ ! "$object_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]]; then
      echo "release tag contains an invalid object ID" >&2
      return 1
    fi
    tag_object="$(
      gh api "repos/$repository/git/tags/$object_sha"
    )" || return 1
    object_type="$(jq -er '.object.type | strings' <<<"$tag_object")" ||
      return 1
    object_sha="$(jq -er '.object.sha | strings' <<<"$tag_object")" ||
      return 1
    tag_depth=$((tag_depth + 1))
  done

  if [[ "$object_type" != "commit" ]]; then
    echo "release tag does not resolve to a commit" >&2
    return 1
  fi
  printf '%s\n' "$object_sha"
}

release_json="$(
  gh api "repos/$repository/releases/$release_id"
)"
if ! matches_state true <<<"$release_json"; then
  fail "GitHub did not return the expected stable draft release"
fi

if [[ -n "$recovery_workflow_sha" ]]; then
  recovery_predicate_type="https://github.com/hunterchen7/viewr/attestations/release-recovery/v1"
  signer_workflow="$repository/.github/workflows/release-binaries.yml"
  while IFS= read -r asset_name; do
    verification="$(
      gh attestation verify \
        "$asset_directory/$asset_name" \
        --repo "$repository" \
        --signer-workflow "$signer_workflow" \
        --signer-digest "$recovery_workflow_sha" \
        --predicate-type "$recovery_predicate_type" \
        --format json
    )"
    if ! jq -e \
      --argjson release_id "$release_id" \
      --arg release_sha "$release_sha" \
      --arg release_tag "$release_tag" \
      --arg workflow_sha "$recovery_workflow_sha" \
      '
        any(.[];
          .verificationResult.statement.predicate.release.id == $release_id
          and .verificationResult.statement.predicate.release.tag == $release_tag
          and .verificationResult.statement.predicate.release.sourceCommit == $release_sha
          and .verificationResult.statement.predicate.workflow.commit == $workflow_sha
        )
      ' <<<"$verification" >/dev/null; then
      fail "recovery attestation identity does not match: $asset_name"
    fi
  done <<<"$actual_assets"
fi

"$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/verify-remote-release-assets.sh" \
  --release-id "$release_id" \
  "$asset_directory" \
  "$release_tag" >/dev/null

object_sha="$(resolve_tag_commit)" ||
  fail "release tag commit could not be resolved"
[[ "$object_sha" == "$release_sha" ]] ||
  fail "release tag moved from $release_sha to $object_sha"
release_json="$(
  gh api "repos/$repository/releases/$release_id"
)"
if ! matches_state true <<<"$release_json"; then
  fail "release changed while its assets were being verified"
fi

publish_response_valid=false
if publish_json="$(
  gh api \
    --method PATCH \
    "repos/$repository/releases/$release_id" \
    -F draft=false \
    -f make_latest=legacy
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
    tag_attempt=1
    while ((tag_attempt <= query_attempts)); do
      current_tag_sha=""
      if current_tag_sha="$(resolve_tag_commit)"; then
        if [[ "$current_tag_sha" == "$release_sha" ]]; then
          echo "Published $repository release $release_tag (ID $release_id)"
          exit 0
        fi
        echo "::error::Release tag moved from $release_sha to $current_tag_sha after publication." >&2
        break
      fi
      if ((tag_attempt < query_attempts)); then
        echo "::warning::Could not recheck the release tag after publication (attempt $tag_attempt/$query_attempts); retrying." >&2
        sleep "$query_delay_seconds"
      fi
      tag_attempt=$((tag_attempt + 1))
    done

    rollback_json=""
    if rollback_json="$(
      gh api \
        --method PATCH \
        "repos/$repository/releases/$release_id" \
        -F draft=true
    )" && matches_state true <<<"$rollback_json"; then
      fail "release tag verification failed after publication; the release was restored to a draft"
    fi
    fail "release tag verification failed after publication and the release could not be restored to a draft"
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
