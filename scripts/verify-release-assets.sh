#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/verify-release-assets.sh --manifest CARGO-TOML <asset-directory> <release-tag>" >&2
}

if [[ $# -ne 4 || "$1" != "--manifest" ]]; then
  usage
  exit 2
fi

manifest_path="$2"
asset_dir="$3"
release_tag="$4"

if [[ ! -f "$manifest_path" ]]; then
  echo "Cargo manifest does not exist: $manifest_path" >&2
  exit 1
fi

version="$(
  cargo metadata \
    --manifest-path "$manifest_path" \
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

actual="$(
  find "$asset_dir" \
    -mindepth 1 \
    -maxdepth 1 \
    -type f \
    ! -name SHA256SUMS \
    -exec basename {} \; |
    LC_ALL=C sort
)"
sorted_expected="$(printf '%s\n' "${expected[@]}" | LC_ALL=C sort)"

if [[ "$actual" != "$sorted_expected" ]]; then
  echo "release asset set does not match the expected files" >&2
  diff \
    --label expected \
    --label actual \
    <(printf '%s\n' "$sorted_expected") \
    <(printf '%s\n' "$actual") >&2 || true
  exit 1
fi

for asset in "${expected[@]}"; do
  if [[ ! -s "$asset_dir/$asset" ]]; then
    echo "release asset is empty: $asset" >&2
    exit 1
  fi
done

if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  checksum_command=(shasum -a 256)
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

(
  cd "$asset_dir"
  "${checksum_command[@]}" "${expected[@]}" >SHA256SUMS
)

echo "$asset_dir/SHA256SUMS"
