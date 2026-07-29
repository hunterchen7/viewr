#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
validator="$repository_root/scripts/validate-release-source-notices.sh"
readonly JPEG_RUSTURBO_SOURCE="registry+https://github.com/rust-lang/crates.io-index"
readonly JPEG_RUSTURBO_CHECKSUM="f99890ec2a56818f0a1783cd6893794637a4fb6b61a3b4394e411d2f4693372f"
temporary_directory="$(
  mktemp -d "${TMPDIR:-/tmp}/viewr-release-notice-test.XXXXXX"
)"
trap 'rm -rf "$temporary_directory"' EXIT

new_source_root() {
  local name="$1"
  local root="$temporary_directory/$name"
  mkdir -p "$root/packaging"
  printf '%s\n' "$root"
}

write_lock() {
  local root="$1"
  shift
  {
    echo 'version = 4'
    for version in "$@"; do
      cat <<EOF

[[package]]
name = "jpeg-rusturbo"
version = "$version"
source = "$JPEG_RUSTURBO_SOURCE"
checksum = "$JPEG_RUSTURBO_CHECKSUM"
EOF
    done
  } >"$root/Cargo.lock"
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

no_jpeg_root="$(new_source_root no-jpeg)"
cat >"$no_jpeg_root/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "unrelated"
version = "1.0.0"
EOF
expect_pass "lock without jpeg-rusturbo" "$no_jpeg_root"

valid_root="$(new_source_root valid)"
write_lock "$valid_root" 0.9.2
copy_valid_notices "$valid_root"
expect_pass "exact jpeg-rusturbo version and notice" "$valid_root"

wrong_version_root="$(new_source_root wrong-version)"
write_lock "$wrong_version_root" 0.9.3
copy_valid_notices "$wrong_version_root"
expect_fail "unsupported jpeg-rusturbo version" "$wrong_version_root"

multiple_versions_root="$(new_source_root multiple-versions)"
write_lock "$multiple_versions_root" 0.9.2 0.9.3
copy_valid_notices "$multiple_versions_root"
expect_fail "multiple jpeg-rusturbo versions" "$multiple_versions_root"

missing_version_root="$(new_source_root missing-version)"
cat >"$missing_version_root/Cargo.lock" <<EOF
version = 4

[[package]]
name = "jpeg-rusturbo"
source = "$JPEG_RUSTURBO_SOURCE"
checksum = "$JPEG_RUSTURBO_CHECKSUM"
EOF
copy_valid_notices "$missing_version_root"
expect_fail "jpeg-rusturbo package without a version" "$missing_version_root"

wrong_source_root="$(new_source_root wrong-source)"
cat >"$wrong_source_root/Cargo.lock" <<EOF
version = 4

[[package]]
name = "jpeg-rusturbo"
version = "0.9.2"
source = "git+https://example.invalid/jpeg-rusturbo"
checksum = "$JPEG_RUSTURBO_CHECKSUM"
EOF
copy_valid_notices "$wrong_source_root"
expect_fail "jpeg-rusturbo package from the wrong source" "$wrong_source_root"

missing_source_root="$(new_source_root missing-source)"
cat >"$missing_source_root/Cargo.lock" <<EOF
version = 4

[[package]]
name = "jpeg-rusturbo"
version = "0.9.2"
checksum = "$JPEG_RUSTURBO_CHECKSUM"
EOF
copy_valid_notices "$missing_source_root"
expect_fail "jpeg-rusturbo package without a source" "$missing_source_root"

wrong_checksum_root="$(new_source_root wrong-checksum)"
cat >"$wrong_checksum_root/Cargo.lock" <<EOF
version = 4

[[package]]
name = "jpeg-rusturbo"
version = "0.9.2"
source = "$JPEG_RUSTURBO_SOURCE"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
EOF
copy_valid_notices "$wrong_checksum_root"
expect_fail "jpeg-rusturbo package with the wrong checksum" "$wrong_checksum_root"

missing_checksum_root="$(new_source_root missing-checksum)"
cat >"$missing_checksum_root/Cargo.lock" <<EOF
version = 4

[[package]]
name = "jpeg-rusturbo"
version = "0.9.2"
source = "$JPEG_RUSTURBO_SOURCE"
EOF
copy_valid_notices "$missing_checksum_root"
expect_fail "jpeg-rusturbo package without a checksum" "$missing_checksum_root"

missing_notices_root="$(new_source_root missing-notices)"
write_lock "$missing_notices_root" 0.9.2
expect_fail "missing third-party notices" "$missing_notices_root"

missing_marker_root="$(new_source_root missing-marker)"
write_lock "$missing_marker_root" 0.9.2
printf 'unrelated notices\n' \
  >"$missing_marker_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "missing jpeg-rusturbo marker" "$missing_marker_root"

duplicate_marker_root="$(new_source_root duplicate-marker)"
write_lock "$duplicate_marker_root" 0.9.2
copy_valid_notices "$duplicate_marker_root"
{
  echo
  echo '----- BEGIN EXACT jpeg-rusturbo 0.9.2 NOTICE.md -----'
} >>"$duplicate_marker_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "duplicate jpeg-rusturbo marker" "$duplicate_marker_root"

changed_tail_root="$(new_source_root changed-tail)"
write_lock "$changed_tail_root" 0.9.2
copy_valid_notices "$changed_tail_root"
printf '\nchanged trailing bytes\n' \
  >>"$changed_tail_root/packaging/THIRD-PARTY-NOTICES.txt"
expect_fail "notice bytes after the terminal block" "$changed_tail_root"

missing_lock_root="$(new_source_root missing-lock)"
expect_fail "missing Cargo.lock" "$missing_lock_root"

echo "Release-source notice policy tests passed."
