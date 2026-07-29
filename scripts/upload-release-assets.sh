#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/upload-release-assets.sh --release-id ID <asset-directory> <release-tag>" >&2
}

fail() {
  echo "error: $*" >&2
  exit 1
}

release_id=""
if [[ "${1:-}" == "--release-id" && $# -ge 2 ]]; then
  release_id="$2"
  shift 2
fi
if [[ $# -ne 2 ]]; then
  usage
  exit 2
fi

asset_directory="$1"
release_tag="$2"
repository="${GITHUB_REPOSITORY:-}"

[[ "$release_id" =~ ^[1-9][0-9]*$ ]] ||
  fail "release ID must be a positive integer"
[[ "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail "release tag is not a stable semantic version: $release_tag"
[[ "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  fail "GITHUB_REPOSITORY must identify one owner and repository"
[[ -n "${GH_TOKEN:-}" ]] || fail "GH_TOKEN is not set"
[[ -d "$asset_directory" ]] ||
  fail "asset directory does not exist: $asset_directory"
command -v curl >/dev/null || fail "curl is required"
command -v gh >/dev/null || fail "gh is required"
command -v jq >/dev/null || fail "jq is required"

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

local_names="$(
  find "$asset_directory" \
    -mindepth 1 \
    -maxdepth 1 \
    -type f \
    -exec basename {} \; |
    LC_ALL=C sort
)"
[[ -n "$local_names" ]] || fail "asset directory is empty"

while IFS= read -r asset_name; do
  [[ "$asset_name" =~ ^[A-Za-z0-9._-]+$ ]] ||
    fail "asset name contains unsupported URL characters: $asset_name"
  [[ -s "$asset_directory/$asset_name" ]] ||
    fail "asset is empty: $asset_name"
done <<<"$local_names"

release_json="$(
  gh api "repos/$repository/releases/$release_id"
)"
if ! jq -e \
  --arg tag "$release_tag" \
  --argjson id "$release_id" \
  '
    .id == $id
    and .tag_name == $tag
    and .draft == true
    and .prerelease == false
    and (.assets | type == "array")
  ' \
  <<<"$release_json" >/dev/null; then
  fail "GitHub did not return the expected stable draft release"
fi

remote_names="$(
  jq -r '.assets[].name' <<<"$release_json" |
    LC_ALL=C sort
)"
unexpected="$(
  comm \
    -13 \
    <(printf '%s\n' "$local_names") \
    <(printf '%s\n' "$remote_names")
)"
if [[ -n "$unexpected" ]]; then
  echo "release contains unexpected existing assets:" >&2
  printf '%s\n' "$unexpected" >&2
  exit 1
fi

temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-release-upload.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT
authorization_header="$temporary_directory/authorization"
upload_response="$temporary_directory/response.json"
printf 'Authorization: Bearer %s\n' "$GH_TOKEN" >"$authorization_header"
chmod 0600 "$authorization_header"

while IFS= read -r asset_name; do
  matching_assets="$(
    jq -c \
      --arg name "$asset_name" \
      '[.assets[] | select(.name == $name)]' \
      <<<"$release_json"
  )"
  match_count="$(jq -r 'length' <<<"$matching_assets")"
  case "$match_count" in
    0) ;;
    1)
      existing_asset_id="$(jq -er '.[0].id | numbers' <<<"$matching_assets")"
      [[ "$existing_asset_id" =~ ^[1-9][0-9]*$ ]] ||
        fail "existing asset has an invalid ID: $asset_name"
      gh api \
        --method DELETE \
        "repos/$repository/releases/assets/$existing_asset_id" \
        --silent
      ;;
    *)
      fail "release contains duplicate assets named $asset_name"
      ;;
  esac

  asset_path="$asset_directory/$asset_name"
  asset_size="$(wc -c <"$asset_path" | tr -d '[:space:]')"
  : >"$upload_response"
  curl \
    --connect-timeout 10 \
    --max-time 600 \
    --fail-with-body \
    --silent \
    --show-error \
    --request POST \
    --header "@$authorization_header" \
    --header "Accept: application/vnd.github+json" \
    --header "X-GitHub-Api-Version: 2026-03-10" \
    --header "Content-Type: application/octet-stream" \
    --data-binary "@$asset_path" \
    --output "$upload_response" \
    "https://uploads.github.com/repos/$repository/releases/$release_id/assets?name=$asset_name"

  if ! jq -e \
    --arg name "$asset_name" \
    --argjson size "$asset_size" \
    '
      (.id | numbers) > 0
      and .name == $name
      and .state == "uploaded"
      and .size == $size
    ' \
    "$upload_response" >/dev/null; then
    fail "GitHub returned an invalid upload response for $asset_name"
  fi
done <<<"$local_names"

echo "Uploaded release assets to $repository release $release_tag (ID $release_id)"
