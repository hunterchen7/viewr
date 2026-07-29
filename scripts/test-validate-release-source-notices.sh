#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
validator="$repository_root/scripts/validate-release-source-notices.sh"
temporary_directory="$(
  mktemp -d "${TMPDIR:-/tmp}/viewr-release-notice-test.XXXXXX"
)"
trap 'rm -rf "$temporary_directory"' EXIT

new_source_root() {
  local name="$1"
  local root="$temporary_directory/$name"
  mkdir -p "$root/packaging"
  printf 'version = 4\n' >"$root/Cargo.lock"
  printf '%s\n' "$root"
}

copy_valid_notices() {
  local root="$1"
  cp \
    "$repository_root/packaging/THIRD-PARTY-NOTICES.txt" \
    "$root/packaging/THIRD-PARTY-NOTICES.txt"
}

expect_pass() {
  local description="$1"
  local root="$2"
  if ! "$validator" "$root" >/dev/null; then
    echo "validator rejected valid fixture: $description" >&2
    exit 1
  fi
}

expect_fail() {
  local description="$1"
  local root="$2"
  if "$validator" "$root" >/dev/null 2>&1; then
    echo "validator accepted invalid fixture: $description" >&2
    exit 1
  fi
}

valid_root="$(new_source_root valid)"
copy_valid_notices "$valid_root"
expect_pass "real source containing the exact notice baseline" "$valid_root"

symlink_root="$temporary_directory/symlink-root"
ln -s "$valid_root" "$symlink_root"
expect_fail "symlinked release source root" "$symlink_root"

missing_lock_root="$(new_source_root missing-lock)"
rm "$missing_lock_root/Cargo.lock"
copy_valid_notices "$missing_lock_root"
expect_fail "missing Cargo.lock" "$missing_lock_root"

symlink_lock_root="$(new_source_root symlink-lock)"
rm "$symlink_lock_root/Cargo.lock"
ln -s "$valid_root/Cargo.lock" "$symlink_lock_root/Cargo.lock"
copy_valid_notices "$symlink_lock_root"
expect_fail "symlinked Cargo.lock" "$symlink_lock_root"

missing_packaging_root="$(new_source_root missing-packaging)"
rmdir "$missing_packaging_root/packaging"
expect_fail "missing packaging directory" "$missing_packaging_root"

symlink_packaging_root="$(new_source_root symlink-packaging)"
rmdir "$symlink_packaging_root/packaging"
ln -s "$valid_root/packaging" "$symlink_packaging_root/packaging"
expect_fail "symlinked packaging directory" "$symlink_packaging_root"

missing_notices_root="$(new_source_root missing-notices)"
expect_fail "missing third-party notices" "$missing_notices_root"

symlink_notices_root="$(new_source_root symlink-notices)"
ln -s \
  "$repository_root/packaging/THIRD-PARTY-NOTICES.txt" \
  "$symlink_notices_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "symlinked third-party notices" "$symlink_notices_root"

missing_marker_root="$(new_source_root missing-marker)"
printf 'unrelated notices\n' \
  >"$missing_marker_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "missing jpeg-rusturbo marker" "$missing_marker_root"

duplicate_marker_root="$(new_source_root duplicate-marker)"
copy_valid_notices "$duplicate_marker_root"
{
  echo
  echo '----- BEGIN EXACT jpeg-rusturbo 0.9.2 NOTICE.md -----'
} >>"$duplicate_marker_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "duplicate jpeg-rusturbo marker" "$duplicate_marker_root"

changed_tail_root="$(new_source_root changed-tail)"
copy_valid_notices "$changed_tail_root"
printf '\nchanged trailing bytes\n' \
  >>"$changed_tail_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "notice bytes after the terminal block" "$changed_tail_root"

arithmetic_injection_root="$(new_source_root 'PATH[$(touch PWNED)]')"
{
  printf '\0'
  cat "$repository_root/packaging/THIRD-PARTY-NOTICES.txt"
} >"$arithmetic_injection_root/packaging/THIRD-PARTY-NOTICES.txt"
(
  cd "$temporary_directory"
  expect_fail \
    "binary preamble with shell metacharacters in the source path" \
    "$arithmetic_injection_root"
)
if [[ -e "$temporary_directory/PWNED" ]]; then
  echo "validator evaluated source-path text as shell arithmetic" >&2
  exit 1
fi

echo "Release-source notice policy tests passed."
