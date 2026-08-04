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
# git archive records submodules as bare gitlinks, so the in-tree rawler fork
# is staged explicitly. The guard rejects a checkout whose submodule drifted
# from the recorded rev or was never initialized, so an archive can never
# silently ship the wrong (or no) rawler source.
recorded_rawler_rev="$(git -C "$repo_root" rev-parse HEAD:thirdparty/dnglab)"
checked_out_rawler_rev="$(git -C "$repo_root/thirdparty/dnglab" rev-parse HEAD 2>/dev/null || true)"
if [[ "$checked_out_rawler_rev" != "$recorded_rawler_rev" ]]; then
  echo "thirdparty/dnglab checkout ($checked_out_rawler_rev) does not match the recorded submodule rev ($recorded_rawler_rev); run 'git submodule update --init'" >&2
  exit 1
fi
# The fork lives outside vendor/ so cargo's directory-source replacement
# never scans it; staging once before vendoring lets cargo resolve the
# workspace's rawler patch target.
mkdir -p "$source_root/thirdparty/dnglab"
git -C "$repo_root/thirdparty/dnglab" archive HEAD | tar -xf - -C "$source_root/thirdparty/dnglab"
(
  cd "$source_root"
  cargo vendor \
    --locked \
    --versioned-dirs \
    --sync tools/jpeg-bakeoff/Cargo.toml \
    --sync thirdparty/dnglab/rawler/Cargo.toml \
    vendor >.cargo/config.toml
)

if touch -h -d "@$source_date_epoch" "$source_root" 2>/dev/null; then
  find "$source_root" -exec touch -h -d "@$source_date_epoch" {} +
else
  source_date_stamp="$(date -r "$source_date_epoch" '+%Y%m%d%H%M.%S')"
  find "$source_root" -exec touch -h -t "$source_date_stamp" {} +
fi

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
