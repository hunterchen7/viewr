#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

readonly JPEG_RUSTURBO_VERSION="0.9.2"
readonly JPEG_RUSTURBO_NOTICE_SHA256="fe5e4bf805fbfb2f4f5443decec492c801722a1b4376eb4878d7edf99cc697eb"
readonly JPEG_RUSTURBO_NOTICE_MARKER="----- BEGIN EXACT jpeg-rusturbo 0.9.2 NOTICE.md -----"

usage() {
  echo "usage: scripts/validate-source-archive.sh <source-archive>" >&2
}

extract_jpeg_rusturbo_notice() {
  local notices_path="$1"
  local destination_path="$2"
  local marker_count
  local marker_position
  local marker_line
  local notice_start_line

  if ! LC_ALL=C tr -d '\000' <"$notices_path" | cmp -s "$notices_path" -; then
    echo "$notices_path contains NUL bytes" >&2
    return 1
  fi

  marker_count="$(
    LC_ALL=C grep -a -Fxc -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path" || true
  )"
  if [[ "$marker_count" != "1" ]]; then
    echo "$notices_path must contain exactly one jpeg-rusturbo NOTICE marker" >&2
    return 1
  fi

  marker_position="$(
    LC_ALL=C grep -a -Fnx -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path"
  )"
  marker_line="${marker_position%%:*}"
  if [[ ! "$marker_line" =~ ^[1-9][0-9]*$ ]]; then
    echo "$notices_path marker has an invalid line number" >&2
    return 1
  fi
  notice_start_line=$((10#$marker_line + 1))
  tail -n "+$notice_start_line" "$notices_path" >"$destination_path"
}

if [[ $# -ne 1 ]]; then
  usage
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archive="$1"
if [[ ! -s "$archive" ]]; then
  echo "source archive is missing or empty: $archive" >&2
  exit 1
fi

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
top_level="viewr-$version"

required=(
  "$top_level/.cargo/config.toml"
  "$top_level/Cargo.lock"
  "$top_level/Cargo.toml"
  "$top_level/packaging/SOURCE-BUILD.md"
  "$top_level/packaging/THIRD-PARTY-NOTICES.txt"
  "$top_level/scripts/prepare-local-rawler.sh"
  "$top_level/tools/jpeg-bakeoff/Cargo.lock"
  "$top_level/tools/jpeg-bakeoff/Cargo.toml"
  "$top_level/vendor/jpeg-encoder-0.6.1/LICENSE-APACHE"
  "$top_level/vendor/jpeg-encoder-0.6.1/LICENSE-MIT"
  "$top_level/vendor/jpeg-rusturbo-0.9.2/NOTICE.md"
  "$top_level/vendor/dnglab/rawler/LICENSE"
  "$top_level/vendor/turbojpeg-sys-1.2.0/libjpeg-turbo/LICENSE.md"
  "$top_level/vendor/turbojpeg-sys-1.2.0/libjpeg-turbo/README.ijg"
)
archive_listing="$(tar -tzf "$archive")"
duplicate_paths="$(
  awk 'seen[$0]++ == 1 { print }' <<<"$archive_listing"
)"
if [[ -n "$duplicate_paths" ]]; then
  echo "source archive contains duplicate member names" >&2
  printf '%s\n' "$duplicate_paths" >&2
  exit 1
fi
unexpected_member_types="$(
  tar -tvzf "$archive" |
    awk 'substr($1, 1, 1) != "-" && substr($1, 1, 1) != "d" { print }'
)"
if [[ -n "$unexpected_member_types" ]]; then
  echo "source archive contains a link or special file" >&2
  printf '%s\n' "$unexpected_member_types" >&2
  exit 1
fi
for path in "${required[@]}"; do
  if ! grep -Fxq "$path" <<<"$archive_listing"; then
    echo "source archive is missing $path" >&2
    exit 1
  fi
done
noncanonical_paths="$(
  awk '$0 ~ /\/\.\.?($|\/)/ || $0 ~ /\/\//' <<<"$archive_listing"
)"
if [[ -n "$noncanonical_paths" ]]; then
  echo "source archive contains a noncanonical path" >&2
  printf '%s\n' "$noncanonical_paths" >&2
  exit 1
fi
forbidden_paths="$(
  awk -v root="$top_level" '
    $0 ~ /(^|\/)\.git(\/|$)/ ||
    $0 == root "/target" ||
    index($0, root "/target/") == 1
  ' <<<"$archive_listing"
)"
if [[ -n "$forbidden_paths" ]]; then
  echo "source archive contains a repository or build-output directory" >&2
  printf '%s\n' "$forbidden_paths" >&2
  exit 1
fi
unexpected_paths="$(
  awk -v root="$top_level" '
    $0 != root && index($0, root "/") != 1
  ' <<<"$archive_listing"
)"
if [[ -n "$unexpected_paths" ]]; then
  echo "source archive contains a path outside $top_level" >&2
  exit 1
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
tar -xzf "$archive" -C "$work_dir"
source_root="$work_dir/$top_level"

if grep -Fq "$work_dir" "$source_root/.cargo/config.toml"; then
  echo "vendored Cargo configuration contains an absolute staging path" >&2
  exit 1
fi
if [[ "$(
  sha256sum "$source_root/vendor/dnglab/rawler/LICENSE" | awk '{print $1}'
)" != "4bb33cc4cd956b56b779b501f18cae46a9e26f8c8500cca86ed758b8bc5e1788" ]]; then
  echo "in-tree rawler fork license does not match the expected LGPL text" >&2
  exit 1
fi
jpeg_rusturbo_notice="$source_root/vendor/jpeg-rusturbo-${JPEG_RUSTURBO_VERSION}/NOTICE.md"
if [[ "$(
  sha256sum "$jpeg_rusturbo_notice" | awk '{print $1}'
)" != "$JPEG_RUSTURBO_NOTICE_SHA256" ]]; then
  echo "vendored jpeg-rusturbo ${JPEG_RUSTURBO_VERSION} NOTICE has an unexpected SHA-256" >&2
  exit 1
fi
packaged_jpeg_rusturbo_notice="$work_dir/packaged-jpeg-rusturbo-NOTICE.md"
extract_jpeg_rusturbo_notice \
  "$source_root/packaging/THIRD-PARTY-NOTICES.txt" \
  "$packaged_jpeg_rusturbo_notice"
if ! cmp -s "$jpeg_rusturbo_notice" "$packaged_jpeg_rusturbo_notice"; then
  echo "source archive third-party notices do not end with the exact jpeg-rusturbo ${JPEG_RUSTURBO_VERSION} NOTICE" >&2
  exit 1
fi

(
  cd "$source_root"
  cargo metadata --locked --offline --format-version 1 >/dev/null
  cargo metadata \
    --manifest-path tools/jpeg-bakeoff/Cargo.toml \
    --locked \
    --offline \
    --format-version 1 >/dev/null
  cargo test \
    --manifest-path tools/jpeg-bakeoff/Cargo.toml \
    --locked \
    --offline
  scripts/prepare-local-rawler.sh
  if [[ -e local/rawler-0.7.2/.cargo-checksum.json ]]; then
    echo "editable rawler copy retained its vendor checksum file" >&2
    exit 1
  fi
  printf '\n// Viewr release-source relink validation.\n' \
    >>local/rawler-0.7.2/src/lib.rs
  rawler_source="$(
    cargo metadata --locked --offline --format-version 1 |
      jq -r '
        [.packages[]
          | select(.name == "rawler" and .version == "0.7.2")
          | .source]
        | if length == 1 then .[0] else error("expected local rawler") end
      '
  )"
  if [[ "$rawler_source" != "null" ]]; then
    echo "Cargo did not select the editable local rawler source" >&2
    exit 1
  fi
  cargo build --release --locked --offline -p viewr --bin viewr
  if [[ ! -x target/release/viewr ]]; then
    echo "offline release build did not produce the Viewr executable" >&2
    exit 1
  fi
)

echo "Validated $archive"
