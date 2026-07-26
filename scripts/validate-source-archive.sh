#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

usage() {
  echo "usage: scripts/validate-source-archive.sh <source-archive>" >&2
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
  "$top_level/scripts/prepare-local-rawler.sh"
  "$top_level/vendor/rawler-0.7.2/LICENSE"
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
  sha256sum "$source_root/vendor/rawler-0.7.2/LICENSE" | awk '{print $1}'
)" != "4bb33cc4cd956b56b779b501f18cae46a9e26f8c8500cca86ed758b8bc5e1788" ]]; then
  echo "vendored rawler 0.7.2 license does not match the package source" >&2
  exit 1
fi

(
  cd "$source_root"
  cargo metadata --locked --offline --format-version 1 >/dev/null
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
  cargo check --locked --offline -p viewr --bin viewr
)

echo "Validated $archive"
