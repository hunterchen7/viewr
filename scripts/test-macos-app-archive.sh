#!/bin/bash
set -euo pipefail

export LC_ALL=C

usage() {
    cat <<'EOF'
Usage: scripts/test-macos-app-archive.sh VIEWR_APP VIEWR_ARCHIVE

Run negative structural and extraction-safety tests for a macOS app archive.
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ $# -eq 2 ]] || {
    usage >&2
    exit 64
}

script_dir="$(cd "$(dirname "$0")" && pwd)"
validator="$script_dir/validate-portable-archive.sh"
app="$1"
archive="$2"

[[ -d "$app" && ! -L "$app" && "$(basename "$app")" == "Viewr.app" ]] ||
    fail "expected app must be a regular directory named Viewr.app"
[[ -s "$archive" && "$(basename "$archive")" == "viewr-macos-arm64.tar.gz" ]] ||
    fail "expected archive must be named viewr-macos-arm64.tar.gz"

for command in gzip ln mkdir mktemp plutil rm tar touch; do
    command -v "$command" >/dev/null || fail "required command is unavailable: $command"
done

"$validator" \
    --platform macos-arm64 \
    --archive "$archive" \
    --expected-app "$app"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/viewr-app-archive-tests.XXXXXXXX")"
cleanup() {
    if [[ -n "${work_dir:-}" && -d "$work_dir" ]]; then
        rm -rf -- "$work_dir"
    fi
}
trap cleanup EXIT

make_archive() {
    local source_dir="$1"
    local output="$2"
    COPYFILE_DISABLE=1 tar \
        --format=ustar \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        -C "$source_dir" \
        -cf - \
        Viewr.app |
        gzip -9n >"$output"
}

expect_validation_failure() {
    local expected_message="$1"
    local invalid_archive="$2"
    local output
    if output="$(
        "$validator" \
            --platform macos-arm64 \
            --archive "$invalid_archive" \
            --expected-app "$app" 2>&1
    )"; then
        fail "invalid archive unexpectedly passed validation: $invalid_archive"
    fi
    if ! grep -Fq "$expected_message" <<<"$output"; then
        echo "$output" >&2
        fail "validator did not report the expected failure: $expected_message"
    fi
}

extra_dir="$work_dir/extra"
mkdir -p "$extra_dir"
tar -xzf "$archive" -C "$extra_dir"
touch "$extra_dir/Viewr.app/Contents/unexpected-file"
extra_archive="$work_dir/extra/viewr-macos-arm64.tar.gz"
make_archive "$extra_dir" "$extra_archive"
expect_validation_failure \
    "portable archive members do not match the exact Viewr.app layout" \
    "$extra_archive"

symlink_dir="$work_dir/symlink"
mkdir -p "$symlink_dir"
tar -xzf "$archive" -C "$symlink_dir"
rm -f -- "$symlink_dir/Viewr.app/Contents/Resources/LICENSE.txt"
ln -s "$app/Contents/Resources/LICENSE.txt" \
    "$symlink_dir/Viewr.app/Contents/Resources/LICENSE.txt"
symlink_archive="$work_dir/symlink/viewr-macos-arm64.tar.gz"
make_archive "$symlink_dir" "$symlink_archive"
expect_validation_failure \
    "archive member is not a 0644 regular file" \
    "$symlink_archive"

metadata_dir="$work_dir/metadata"
mkdir -p "$metadata_dir"
tar -xzf "$archive" -C "$metadata_dir"
plutil -replace CFBundlePackageType \
    -string BNDL \
    "$metadata_dir/Viewr.app/Contents/Info.plist"
metadata_archive="$work_dir/metadata/viewr-macos-arm64.tar.gz"
make_archive "$metadata_dir" "$metadata_archive"
expect_validation_failure \
    "app metadata differs from the exact versioned Info.plist template" \
    "$metadata_archive"

duplicate_dir="$work_dir/duplicate"
mkdir -p "$duplicate_dir"
tar -xzf "$archive" -C "$duplicate_dir"
duplicate_tar="$work_dir/duplicate.tar"
COPYFILE_DISABLE=1 tar \
    --format=ustar \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$duplicate_dir" \
    -cf "$duplicate_tar" \
    Viewr.app
COPYFILE_DISABLE=1 tar \
    --format=ustar \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$duplicate_dir" \
    -rf "$duplicate_tar" \
    Viewr.app/Contents/Info.plist
duplicate_archive="$work_dir/duplicate/viewr-macos-arm64.tar.gz"
gzip -9n <"$duplicate_tar" >"$duplicate_archive"
expect_validation_failure \
    "portable archive members do not match the exact Viewr.app layout" \
    "$duplicate_archive"

echo "Passed macOS app archive negative tests"
