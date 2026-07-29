#!/usr/bin/env bash
set -euo pipefail

readonly JPEG_RUSTURBO_VERSION="0.9.2"
readonly JPEG_RUSTURBO_SOURCE="registry+https://github.com/rust-lang/crates.io-index"
readonly JPEG_RUSTURBO_CHECKSUM="f99890ec2a56818f0a1783cd6893794637a4fb6b61a3b4394e411d2f4693372f"
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
lock_path="$source_root/Cargo.lock"
notices_path="$source_root/packaging/THIRD-PARTY-NOTICES.txt"

[[ -d "$source_root" ]] || fail "release source root does not exist: $source_root"
[[ -f "$lock_path" ]] || fail "release source Cargo.lock is missing: $lock_path"

jpeg_rusturbo_versions="$(
  awk '
    function emit_package() {
      if (in_package && package_name == "jpeg-rusturbo") {
        if (package_version == "") {
          package_version = "<missing-version>"
        }
        if (package_source == "") {
          package_source = "<missing-source>"
        }
        if (package_checksum == "") {
          package_checksum = "<missing-checksum>"
        }
        print package_version "|" package_source "|" package_checksum
      }
    }

    /^\[\[package\]\][[:space:]]*$/ {
      emit_package()
      in_package = 1
      package_name = ""
      package_version = ""
      package_source = ""
      package_checksum = ""
      next
    }

    in_package && /^name = "[^"]+"[[:space:]]*$/ {
      package_name = $0
      sub(/^name = "/, "", package_name)
      sub(/"[[:space:]]*$/, "", package_name)
      next
    }

    in_package && /^source = "[^"]+"[[:space:]]*$/ {
      package_source = $0
      sub(/^source = "/, "", package_source)
      sub(/"[[:space:]]*$/, "", package_source)
      next
    }

    in_package && /^checksum = "[^"]+"[[:space:]]*$/ {
      package_checksum = $0
      sub(/^checksum = "/, "", package_checksum)
      sub(/"[[:space:]]*$/, "", package_checksum)
      next
    }

    in_package && /^version = "[^"]+"[[:space:]]*$/ {
      package_version = $0
      sub(/^version = "/, "", package_version)
      sub(/"[[:space:]]*$/, "", package_version)
      next
    }

    END {
      emit_package()
    }
  ' "$lock_path"
)"

if [[ -z "$jpeg_rusturbo_versions" ]]; then
  echo "Release source does not lock jpeg-rusturbo; no JPEG notice is required."
  exit 0
fi

jpeg_rusturbo_count="$(
  awk 'NF { count += 1 } END { print count + 0 }' \
    <<<"$jpeg_rusturbo_versions"
)"
if [[ "$jpeg_rusturbo_count" != "1" ]]; then
  fail "release source must lock at most one jpeg-rusturbo version; found $jpeg_rusturbo_count"
fi

IFS='|' read -r locked_version locked_source locked_checksum \
  <<<"$jpeg_rusturbo_versions"
[[ "$locked_version" == "$JPEG_RUSTURBO_VERSION" ]] ||
  fail "release source locks unsupported jpeg-rusturbo version: $locked_version"
[[ "$locked_source" == "$JPEG_RUSTURBO_SOURCE" ]] ||
  fail "release source locks jpeg-rusturbo from an unsupported source: $locked_source"
[[ "$locked_checksum" == "$JPEG_RUSTURBO_CHECKSUM" ]] ||
  fail "release source locks jpeg-rusturbo with an unexpected checksum: $locked_checksum"

[[ -f "$notices_path" ]] ||
  fail "release source third-party notices are missing: $notices_path"

marker_count="$(
  grep -Fxc -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path" || true
)"
if [[ "$marker_count" != "1" ]]; then
  fail "release source notices must contain exactly one jpeg-rusturbo 0.9.2 marker"
fi

marker_position="$(
  grep -Fnx -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path"
)"
marker_line="${marker_position%%:*}"

if command -v sha256sum >/dev/null 2>&1; then
  actual_notice_sha256="$(
    tail -n "+$((marker_line + 1))" "$notices_path" |
      sha256sum |
      awk '{ print $1 }'
  )"
elif command -v shasum >/dev/null 2>&1; then
  actual_notice_sha256="$(
    tail -n "+$((marker_line + 1))" "$notices_path" |
      shasum -a 256 |
      awk '{ print $1 }'
  )"
else
  fail "sha256sum or shasum is required"
fi

if [[ "$actual_notice_sha256" != "$JPEG_RUSTURBO_NOTICE_SHA256" ]]; then
  fail "release source notices do not end with the exact jpeg-rusturbo 0.9.2 NOTICE"
fi

echo "Release source contains the required jpeg-rusturbo 0.9.2 notice."
