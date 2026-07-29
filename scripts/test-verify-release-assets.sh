#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "error: $*" >&2
  exit 1
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for command in awk cargo jq mktemp wc; do
  command -v "$command" >/dev/null 2>&1 ||
    fail "required command is unavailable: $command"
done

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
expected=(
  "viewr-$version-source.tar.gz"
  "viewr-linux-x64.deb"
  "viewr-linux-x64.tar.gz"
  "viewr-macos-arm64.pkg"
  "viewr-macos-arm64.tar.gz"
  "viewr-windows-x64.msi"
  "viewr-windows-x64.zip"
)

temporary_directory="$(mktemp -d)"
cleanup() {
  rm -rf "$temporary_directory"
}
trap cleanup EXIT
asset_directory="$temporary_directory/assets"
mkdir -p "$asset_directory"

for asset in "${expected[@]}"; do
  printf 'synthetic release asset: %s\n' "$asset" >"$asset_directory/$asset"
done

manifest="$(
  "$repo_root/scripts/verify-release-assets.sh" \
    --manifest "$repo_root/Cargo.toml" \
    "$asset_directory" \
    "v$version"
)"
[[ "$manifest" == "$asset_directory/SHA256SUMS" ]] ||
  fail "release verifier reported an unexpected checksum path"
[[ "$(wc -l <"$manifest" | tr -d '[:space:]')" == "${#expected[@]}" ]] ||
  fail "checksum manifest does not contain one entry per release asset"
expected_names="$(printf '%s\n' "${expected[@]}")"
actual_names="$(awk '{ print $2 }' "$manifest")"
[[ "$actual_names" == "$expected_names" ]] ||
  fail "checksum manifest asset order does not match the release contract"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$asset_directory" && sha256sum --check SHA256SUMS >/dev/null)
elif command -v shasum >/dev/null 2>&1; then
  (cd "$asset_directory" && shasum -a 256 --check SHA256SUMS >/dev/null)
else
  fail "sha256sum or shasum is required"
fi

assert_rejected() {
  if "$repo_root/scripts/verify-release-assets.sh" \
    --manifest "$repo_root/Cargo.toml" \
    "$asset_directory" \
    "$1" >/dev/null 2>&1
  then
    fail "$2"
  fi
}

printf 'unexpected\n' >"$asset_directory/unexpected.bin"
assert_rejected "v$version" "release verifier accepted an unexpected asset"
rm "$asset_directory/unexpected.bin"

missing_asset="${expected[0]}"
rm "$asset_directory/$missing_asset"
assert_rejected "v$version" "release verifier accepted a missing asset"
printf 'synthetic release asset: %s\n' "$missing_asset" \
  >"$asset_directory/$missing_asset"

empty_asset="${expected[1]}"
: >"$asset_directory/$empty_asset"
assert_rejected "v$version" "release verifier accepted an empty asset"
printf 'synthetic release asset: %s\n' "$empty_asset" \
  >"$asset_directory/$empty_asset"

assert_rejected "v999.999.999" "release verifier accepted a mismatched tag"

echo "Release asset-set verification tests passed."
