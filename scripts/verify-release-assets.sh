#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/verify-release-assets.sh <asset-directory> <release-tag>" >&2
}

if [[ $# -ne 2 ]]; then
  usage
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
asset_dir="$1"
release_tag="$2"

version="$(
  cargo metadata \
    --manifest-path "$repo_root/Cargo.toml" \
    --locked \
    --no-deps \
    --format-version 1 |
    jq -er '
      [.packages[] | select(.name == "viewr") | .version]
      | if length == 1 then .[0] else error("expected one viewr package") end
    '
)"

if [[ "$release_tag" != "v$version" ]]; then
  echo "release tag $release_tag does not match Cargo version v$version" >&2
  exit 1
fi

expected=(
  "viewr-$version-source.tar.gz"
  "viewr-linux-x64.deb"
  "viewr-linux-x64.tar.gz"
  "viewr-macos-arm64.pkg"
  "viewr-macos-arm64.tar.gz"
  "viewr-windows-x64.msi"
  "viewr-windows-x64.zip"
)

mapfile -t actual < <(
  find "$asset_dir" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort
)
mapfile -t sorted_expected < <(printf '%s\n' "${expected[@]}" | LC_ALL=C sort)

if [[ "${actual[*]}" != "${sorted_expected[*]}" ]]; then
  echo "release asset set does not match the expected files" >&2
  diff \
    --label expected \
    --label actual \
    <(printf '%s\n' "${sorted_expected[@]}") \
    <(printf '%s\n' "${actual[@]}") >&2 || true
  exit 1
fi

for asset in "${expected[@]}"; do
  if [[ ! -s "$asset_dir/$asset" ]]; then
    echo "release asset is empty: $asset" >&2
    exit 1
  fi
done

(
  cd "$asset_dir"
  sha256sum "${sorted_expected[@]}" >SHA256SUMS
)

echo "$asset_dir/SHA256SUMS"
