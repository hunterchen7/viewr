#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C

usage() {
    cat <<'EOF'
Usage: scripts/package-portable-archive.sh \
  --platform macos-arm64|linux-x64 \
  --binary PATH \
  --output PATH

Build a Viewr portable tar.gz archive for the current native platform.
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 ||
        fail "required command is unavailable: $1"
}

platform=""
binary_path=""
output_path=""

while (($# > 0)); do
    case "$1" in
        --platform)
            (($# >= 2)) || fail "--platform requires a value"
            platform="$2"
            shift 2
            ;;
        --binary)
            (($# >= 2)) || fail "--binary requires a path"
            binary_path="$2"
            shift 2
            ;;
        --output)
            (($# >= 2)) || fail "--output requires a path"
            output_path="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

[[ -n "$platform" ]] || fail "--platform is required"
[[ -n "$binary_path" ]] || fail "--binary is required"
[[ -n "$output_path" ]] || fail "--output is required"

for command_name in \
    awk basename cat chmod dirname grep gzip install mkdir mktemp mv rm tar touch uname
do
    require_command "$command_name"
done

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repository_root="$(cd -- "${script_directory}/.." && pwd -P)"

[[ -f "$binary_path" && -x "$binary_path" ]] ||
    fail "Viewr binary is not an executable file: $binary_path"
binary_directory="$(cd -- "$(dirname -- "$binary_path")" && pwd -P)"
binary_path="${binary_directory}/$(basename -- "$binary_path")"

case "$platform" in
    macos-arm64)
        [[ "$(uname -s)" == "Darwin" ]] ||
            fail "macos-arm64 archives must be built on macOS"
        [[ "$(basename -- "$output_path")" == "viewr-macos-arm64.tar.gz" ]] ||
            fail "macos-arm64 output must be named viewr-macos-arm64.tar.gz"
        require_command lipo
        require_command shasum
        [[ "$(lipo -archs "$binary_path")" == "arm64" ]] ||
            fail "Viewr binary must contain only arm64 code"
        ;;
    linux-x64)
        [[ "$(uname -s)" == "Linux" ]] ||
            fail "linux-x64 archives must be built on Linux"
        [[ "$(basename -- "$output_path")" == "viewr-linux-x64.tar.gz" ]] ||
            fail "linux-x64 output must be named viewr-linux-x64.tar.gz"
        require_command readelf
        require_command sha256sum
        elf_header="$(readelf -h "$binary_path")"
        grep -Fq 'Class:                             ELF64' <<<"$elf_header" ||
            fail "Viewr binary is not ELF64"
        grep -Fq 'Data:                              2'\''s complement, little endian' \
            <<<"$elf_header" ||
            fail "Viewr binary is not little-endian"
        grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' \
            <<<"$elf_header" ||
            fail "Viewr binary is not x86-64"
        ;;
    *)
        fail "unsupported platform: $platform"
        ;;
esac

archive_members=(
    "viewr"
    "LICENSE"
    "THIRD-PARTY-LICENSES.txt"
    "THIRD-PARTY-NOTICES.txt"
    "RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
    "SOURCE-BUILD.md"
    "rawler-0.7.2-LICENSE"
)
source_files=(
    "$binary_path"
    "$repository_root/LICENSE"
    "$repository_root/packaging/THIRD-PARTY-LICENSES.txt"
    "$repository_root/packaging/THIRD-PARTY-NOTICES.txt"
    "$repository_root/packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
    "$repository_root/packaging/SOURCE-BUILD.md"
    "$repository_root/packaging/licenses/rawler-0.7.2-LICENSE"
)

for source_file in "${source_files[@]}"; do
    [[ -s "$source_file" ]] || fail "required source file is missing or empty: $source_file"
done

expected_rawler_license_sha256="c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"
if [[ "$platform" == "macos-arm64" ]]; then
    actual_rawler_license_sha256="$(
        shasum -a 256 "${source_files[6]}" | awk '{ print $1 }'
    )"
else
    actual_rawler_license_sha256="$(
        sha256sum "${source_files[6]}" | awk '{ print $1 }'
    )"
fi
[[ "$actual_rawler_license_sha256" == "$expected_rawler_license_sha256" ]] ||
    fail "rawler 0.7.2 LICENSE has an unexpected SHA-256"

output_directory="$(dirname -- "$output_path")"
mkdir -p -- "$output_directory"
output_directory="$(cd -- "$output_directory" && pwd -P)"
output_path="${output_directory}/$(basename -- "$output_path")"

temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-portable.XXXXXXXX")"
cleanup() {
    if [[ -n "${temporary_archive:-}" && -f "$temporary_archive" ]]; then
        rm -f -- "$temporary_archive"
    fi
    if [[ -n "${temporary_directory:-}" && -d "$temporary_directory" ]]; then
        rm -rf -- "$temporary_directory"
    fi
}
trap cleanup EXIT

stage_directory="${temporary_directory}/stage"
install -d -m 0755 "$stage_directory"
install -m 0755 "${source_files[0]}" "${stage_directory}/${archive_members[0]}"

for index in 1 2 3 4 5 6; do
    install -m 0644 \
        "${source_files[$index]}" \
        "${stage_directory}/${archive_members[$index]}"
done

TZ=UTC touch -t 198001010000.00 \
    "${archive_members[@]/#/${stage_directory}/}"

temporary_archive="$(mktemp "${output_directory}/.viewr-portable.XXXXXXXX")"
COPYFILE_DISABLE=1 tar \
    --format=ustar \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$stage_directory" \
    -cf - \
    "${archive_members[@]}" |
    gzip -9n >"$temporary_archive"
chmod 0644 "$temporary_archive"
mv -f -- "$temporary_archive" "$output_path"
temporary_archive=""

[[ -s "$output_path" ]] || fail "archive was not created: $output_path"
printf 'Created %s\n' "$output_path"
