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
command -v jq >/dev/null 2>&1 || {
  echo "jq is required" >&2
  exit 1
}

local_names="$(
  find "$asset_dir" -mindepth 1 -maxdepth 1 -type f -exec basename {} \; |
    LC_ALL=C sort
)"
remote_json="$(
  gh release view \
    "$release_tag" \
    --repo "$repository" \
    --json assets
)"
if ! jq -e '.assets | type == "array"' <<< "$remote_json" >/dev/null; then
  echo "GitHub did not return a release asset array" >&2
  exit 1
fi
remote_names="$(
  jq -r '.assets[].name' <<< "$remote_json" |
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

if [[ "$allow_missing" == "0" && -n "$local_names" ]]; then
  if command -v sha256sum >/dev/null 2>&1; then
    checksum_command=(sha256sum)
  elif command -v shasum >/dev/null 2>&1; then
    checksum_command=(shasum -a 256)
  else
    echo "sha256sum or shasum is required" >&2
    exit 1
  fi

  while IFS= read -r asset_name; do
    asset_json="$(
      jq -cer \
        --arg name "$asset_name" \
        '
          [.assets[] | select(.name == $name)]
          | if length == 1 then .[0]
            else error("expected one matching remote release asset")
            end
        ' \
        <<< "$remote_json"
    )"
    remote_state="$(jq -er '.state' <<< "$asset_json")"
    remote_size="$(jq -er '.size' <<< "$asset_json")"
    remote_digest="$(jq -er '.digest | strings' <<< "$asset_json")"
    local_size="$(wc -c < "$asset_dir/$asset_name" | tr -d '[:space:]')"
    local_digest="sha256:$("${checksum_command[@]}" "$asset_dir/$asset_name" | awk '{print $1}')"

    if [[ "$remote_state" != "uploaded" ]]; then
      echo "remote release asset is not uploaded: $asset_name ($remote_state)" >&2
      exit 1
    fi
    if [[ "$remote_size" != "$local_size" ]]; then
      echo "remote release asset size does not match: $asset_name" >&2
      exit 1
    fi
    if [[ "$remote_digest" != "$local_digest" ]]; then
      echo "remote release asset digest does not match: $asset_name" >&2
      exit 1
    fi
  done <<< "$local_names"
fi

echo "Verified remote assets for $repository release $release_tag"
