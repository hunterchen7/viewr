#!/usr/bin/env bash
set -euo pipefail

readonly JPEG_RUSTURBO_NOTICE_SHA256="fe5e4bf805fbfb2f4f5443decec492c801722a1b4376eb4878d7edf99cc697eb"
readonly JPEG_RUSTURBO_NOTICE_MARKER="----- BEGIN EXACT jpeg-rusturbo 0.9.2 NOTICE.md -----"

fail() {
  echo "error: $*" >&2
  exit 1
}

if [[ $# -ne 1 ]]; then
  fail "usage: scripts/validate-release-source-notices.sh SOURCE-ROOT"
fi

source_root="$1"
[[ -d "$source_root" ]] ||
  fail "release source root must be a directory: $source_root"
logical_source_root="$(cd -- "$source_root" && pwd -L)"
physical_source_root="$(cd -- "$source_root" && pwd -P)"
[[ "$logical_source_root" == "$physical_source_root" ]] ||
  fail "release source root must not traverse symbolic links: $source_root"
source_root="$physical_source_root"
lock_path="$source_root/Cargo.lock"
packaging_path="$source_root/packaging"
notices_path="$source_root/packaging/THIRD-PARTY-NOTICES.txt"

[[ -f "$lock_path" && ! -L "$lock_path" ]] ||
  fail "release source Cargo.lock must be a regular file: $lock_path"

[[ -d "$packaging_path" && ! -L "$packaging_path" ]] ||
  fail "release source packaging path must be a real directory: $packaging_path"
[[ -f "$notices_path" && ! -L "$notices_path" ]] ||
  fail "release source third-party notices must be a regular file: $notices_path"
if ! LC_ALL=C tr -d '\000' <"$notices_path" | cmp -s "$notices_path" -; then
  fail "release source third-party notices must not contain NUL bytes"
fi

marker_count="$(
  LC_ALL=C grep -a -Fxc -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path" || true
)"
if [[ "$marker_count" != "1" ]]; then
  fail "release source notices must contain exactly one jpeg-rusturbo 0.9.2 marker"
fi

marker_position="$(
  LC_ALL=C grep -a -Fnx -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path"
)"
marker_line="${marker_position%%:*}"
[[ "$marker_line" =~ ^[1-9][0-9]*$ ]] ||
  fail "release source JPEG notice marker has an invalid line number"
notice_start_line=$((10#$marker_line + 1))

if command -v sha256sum >/dev/null 2>&1; then
  actual_notice_sha256="$(
    tail -n "+$notice_start_line" "$notices_path" |
      sha256sum |
      awk '{ print $1 }'
  )"
elif command -v shasum >/dev/null 2>&1; then
  actual_notice_sha256="$(
    tail -n "+$notice_start_line" "$notices_path" |
      shasum -a 256 |
      awk '{ print $1 }'
  )"
else
  fail "sha256sum or shasum is required"
fi

if [[ "$actual_notice_sha256" != "$JPEG_RUSTURBO_NOTICE_SHA256" ]]; then
  fail "release source notices do not end with the exact jpeg-rusturbo 0.9.2 NOTICE"
fi

echo "Release source contains the required pinned JPEG notice baseline."
