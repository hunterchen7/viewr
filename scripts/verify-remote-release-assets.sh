#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/verify-remote-release-assets.sh [--allow-missing] <asset-directory> <release-tag>" >&2
}

allow_missing=0
if [[ "${1:-}" == "--allow-missing" ]]; then
  allow_missing=1
  shift
fi
if [[ $# -ne 2 ]]; then
  usage
  exit 2
fi

asset_dir="$1"
release_tag="$2"
repository="${GITHUB_REPOSITORY:-}"

if [[ ! -d "$asset_dir" ]]; then
  echo "asset directory does not exist: $asset_dir" >&2
  exit 1
fi
if [[ -z "$release_tag" ]]; then
  echo "release tag must not be empty" >&2
  exit 1
fi
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "GITHUB_REPOSITORY must identify one owner and repository" >&2
  exit 1
fi
command -v gh >/dev/null 2>&1 || {
  echo "gh is required" >&2
  exit 1
}

local_names="$(
  find "$asset_dir" -mindepth 1 -maxdepth 1 -type f -exec basename {} \; |
    LC_ALL=C sort
)"
remote_names="$(
  gh release view \
    "$release_tag" \
    --repo "$repository" \
    --json assets \
    --jq '.assets[].name' |
    LC_ALL=C sort
)"

if [[ "$allow_missing" == "1" ]]; then
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
elif [[ "$local_names" != "$remote_names" ]]; then
  echo "published release assets do not match the validated local set" >&2
  diff \
    --label local \
    --label remote \
    <(printf '%s\n' "$local_names") \
    <(printf '%s\n' "$remote_names") >&2 || true
  exit 1
fi

echo "Verified remote assets for $repository release $release_tag"
