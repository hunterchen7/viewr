#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendor_source="$repo_root/vendor/rawler-0.7.2"
local_root="$repo_root/local"
local_source="$local_root/rawler-0.7.2"
manifest="$repo_root/Cargo.toml"

if [[ ! -d "$vendor_source" ]]; then
  echo "vendored rawler source is missing: $vendor_source" >&2
  echo "run this script from an extracted Viewr release source archive" >&2
  exit 1
fi
if [[ -e "$local_source" ]]; then
  echo "local rawler source already exists: $local_source" >&2
  exit 1
fi
if grep -Eq '^\[patch\.crates-io\][[:space:]]*$' "$manifest"; then
  echo "Cargo.toml already contains a [patch.crates-io] table" >&2
  exit 1
fi

mkdir -p "$local_root"
cp -R "$vendor_source" "$local_source"
chmod -R u+w "$local_source"
rm -f "$local_source/.cargo-checksum.json"

printf '\n[patch.crates-io]\nrawler = { path = "local/rawler-0.7.2" }\n' \
  >>"$manifest"
cargo update \
  --manifest-path "$manifest" \
  --offline \
  -p rawler@0.7.2

echo "Prepared editable rawler source at $local_source"
