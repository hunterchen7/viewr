#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/package-source-archive.sh <output-directory>" >&2
}

if [[ $# -ne 1 ]]; then
  usage
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="$1"
mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"

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

if [[ -n "${VIEWR_RELEASE_TAG:-}" && "$VIEWR_RELEASE_TAG" != "v$version" ]]; then
  echo "release tag $VIEWR_RELEASE_TAG does not match Cargo version v$version" >&2
  exit 1
fi

source_date_epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo_root" log -1 --format=%ct)}"
if [[ ! "$source_date_epoch" =~ ^[0-9]+$ ]]; then
  echo "SOURCE_DATE_EPOCH must contain only decimal digits" >&2
  exit 1
fi

stage_root="$(mktemp -d)"
trap 'rm -rf "$stage_root"' EXIT
source_root="$stage_root/viewr-$version"
mkdir -p "$source_root/.cargo"

git -C "$repo_root" archive HEAD | tar -xf - -C "$source_root"
(
  cd "$source_root"
  printf '\n' >>.cargo/config.toml
  cargo vendor --locked --versioned-dirs vendor >>.cargo/config.toml
)

find "$source_root" -exec touch -h -d "@$source_date_epoch" {} +

archive="$output_dir/viewr-$version-source.tar.gz"
tar \
  --sort=name \
  --mtime="@$source_date_epoch" \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  --pax-option=delete=atime,delete=ctime \
  -C "$stage_root" \
  -cf - \
  "viewr-$version" |
  gzip -9n >"$archive"

echo "$archive"
