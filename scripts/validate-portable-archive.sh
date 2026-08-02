#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C

usage() {
    cat <<'EOF'
Usage: scripts/validate-portable-archive.sh \
  --platform macos-arm64|linux-x64 \
  --archive PATH \
  (--expected-app PATH | --expected-binary PATH)

Validate an exact Viewr portable tar.gz archive.
Use --expected-app for macOS and --expected-binary for Linux.
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
archive_path=""
expected_app_path=""
expected_binary_path=""

while (($# > 0)); do
    case "$1" in
        --platform)
            (($# >= 2)) || fail "--platform requires a value"
            platform="$2"
            shift 2
            ;;
        --archive)
            (($# >= 2)) || fail "--archive requires a path"
            archive_path="$2"
            shift 2
            ;;
        --expected-binary)
            (($# >= 2)) || fail "--expected-binary requires a path"
            expected_binary_path="$2"
            shift 2
            ;;
        --expected-app)
            (($# >= 2)) || fail "--expected-app requires a path"
            expected_app_path="$2"
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
[[ -n "$archive_path" ]] || fail "--archive is required"

for command_name in \
    awk basename cat cmp diff dirname grep mktemp rm sort tar uname
do
    require_command "$command_name"
done

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repository_root="$(cd -- "${script_directory}/.." && pwd -P)"

[[ -s "$archive_path" ]] || fail "archive is missing or empty: $archive_path"
archive_directory="$(cd -- "$(dirname -- "$archive_path")" && pwd -P)"
archive_path="${archive_directory}/$(basename -- "$archive_path")"

case "$platform" in
    macos-arm64)
        [[ "$(uname -s)" == "Darwin" ]] ||
            fail "macos-arm64 archives must be validated on macOS"
        [[ "$(basename -- "$archive_path")" == "viewr-macos-arm64.tar.gz" ]] ||
            fail "macos-arm64 archive must be named viewr-macos-arm64.tar.gz"
        [[ -n "$expected_app_path" ]] ||
            fail "--expected-app is required for macos-arm64"
        [[ -z "$expected_binary_path" ]] ||
            fail "--expected-binary is not valid for macos-arm64"
        require_command paste
        [[ -d "$expected_app_path" && ! -L "$expected_app_path" ]] ||
            fail "expected app is not a regular directory: $expected_app_path"
        expected_app_directory="$(
            cd -- "$(dirname -- "$expected_app_path")" && pwd -P
        )"
        expected_app_path="${expected_app_directory}/$(basename -- "$expected_app_path")"
        "$repository_root/scripts/validate-macos-app.sh" "$expected_app_path"
        ;;
    linux-x64)
        [[ "$(uname -s)" == "Linux" ]] ||
            fail "linux-x64 archives must be validated on Linux"
        [[ "$(basename -- "$archive_path")" == "viewr-linux-x64.tar.gz" ]] ||
            fail "linux-x64 archive must be named viewr-linux-x64.tar.gz"
        [[ -n "$expected_binary_path" ]] ||
            fail "--expected-binary is required for linux-x64"
        [[ -z "$expected_app_path" ]] ||
            fail "--expected-app is not valid for linux-x64"
        [[ -f "$expected_binary_path" && -x "$expected_binary_path" ]] ||
            fail "expected binary is not an executable file: $expected_binary_path"
        expected_binary_directory="$(
            cd -- "$(dirname -- "$expected_binary_path")" && pwd -P
        )"
        expected_binary_path="$(
            printf '%s/%s' \
                "$expected_binary_directory" \
                "$(basename -- "$expected_binary_path")"
        )"
        require_command readelf
        require_command sha256sum
        ;;
    *)
        fail "unsupported platform: $platform"
        ;;
esac

temporary_directory="$(
    mktemp -d "${TMPDIR:-/tmp}/viewr-portable-validation.XXXXXXXX"
)"
cleanup() {
    if [[ -n "${temporary_directory:-}" && -d "$temporary_directory" ]]; then
        rm -rf -- "$temporary_directory"
    fi
}
trap cleanup EXIT

if [[ "$platform" == "macos-arm64" ]]; then
    app_directories=(
        "Viewr.app/"
        "Viewr.app/Contents/"
        "Viewr.app/Contents/MacOS/"
        "Viewr.app/Contents/Resources/"
        "Viewr.app/Contents/_CodeSignature/"
    )
    app_executables=(
        "Viewr.app/Contents/MacOS/ViewrLauncher"
        "Viewr.app/Contents/MacOS/viewr-bin"
    )
    app_data_files=(
        "Viewr.app/Contents/Info.plist"
        "Viewr.app/Contents/PkgInfo"
        "Viewr.app/Contents/Resources/LICENSE.txt"
        "Viewr.app/Contents/Resources/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
        "Viewr.app/Contents/Resources/SOURCE-BUILD.md"
        "Viewr.app/Contents/Resources/THIRD-PARTY-LICENSES.txt"
        "Viewr.app/Contents/Resources/THIRD-PARTY-NOTICES.txt"
        "Viewr.app/Contents/Resources/rawler-LICENSE.txt"
        "Viewr.app/Contents/_CodeSignature/CodeResources"
    )
    archive_members=(
        "${app_directories[@]}"
        "${app_executables[@]}"
        "${app_data_files[@]}"
    )

    member_names="$temporary_directory/archive-member-names.txt"
    member_details="$temporary_directory/archive-member-details.txt"
    tar -tzf "$archive_path" >"$member_names"
    tar -tvzf "$archive_path" |
        awk '{ print $1 "\t" $3 "\t" $4 }' >"$member_details"

    actual_members="$(sort "$member_names")"
    expected_members="$(printf '%s\n' "${archive_members[@]}" | sort)"
    if [[ "$actual_members" != "$expected_members" ]]; then
        echo "error: portable archive members do not match the exact Viewr.app layout" >&2
        diff \
            --label expected \
            --label actual \
            <(printf '%s\n' "$expected_members") \
            <(printf '%s\n' "$actual_members") >&2 || true
        exit 1
    fi

    while IFS=$'\t' read -r member mode owner group; do
        [[ "$owner" == "0" && "$group" == "0" ]] ||
            fail "archive member owner is not 0:0: $member"
        case "$member" in
            */)
                [[ "$mode" == "drwxr-xr-x" ]] ||
                    fail "archive member is not a 0755 directory: $member"
                ;;
            "${app_executables[0]}"|"${app_executables[1]}")
                [[ "$mode" == "-rwxr-xr-x" ]] ||
                    fail "archive member is not a 0755 regular file: $member"
                ;;
            *)
                [[ "$mode" == "-rw-r--r--" ]] ||
                    fail "archive member is not a 0644 regular file: $member"
                ;;
        esac
    done < <(paste "$member_names" "$member_details")

    tar -xzf "$archive_path" -C "$temporary_directory"
    extracted_app="$temporary_directory/Viewr.app"
    "$repository_root/scripts/validate-macos-app.sh" "$extracted_app"
    diff -qr "$expected_app_path" "$extracted_app" >/dev/null ||
        fail "archive Viewr.app differs from its source app"
else
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
        "$expected_binary_path"
        "$repository_root/LICENSE"
        "$repository_root/packaging/THIRD-PARTY-LICENSES.txt"
        "$repository_root/packaging/THIRD-PARTY-NOTICES.txt"
        "$repository_root/packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
        "$repository_root/packaging/SOURCE-BUILD.md"
        "$repository_root/packaging/licenses/rawler-0.7.2-LICENSE"
    )

    for source_file in "${source_files[@]}"; do
        [[ -s "$source_file" ]] ||
            fail "required comparison file is missing or empty: $source_file"
    done

    expected_rawler_license_sha256="c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"
    actual_rawler_license_sha256="$(
        sha256sum "${source_files[6]}" | awk '{ print $1 }'
    )"
    [[ "$actual_rawler_license_sha256" == "$expected_rawler_license_sha256" ]] ||
        fail "rawler 0.7.2 LICENSE has an unexpected SHA-256"

    actual_members="$(tar -tzf "$archive_path" | sort)"
    expected_members="$(printf '%s\n' "${archive_members[@]}" | sort)"
    if [[ "$actual_members" != "$expected_members" ]]; then
        echo "error: portable archive members do not match the expected files" >&2
        diff \
            --label expected \
            --label actual \
            <(printf '%s\n' "$expected_members") \
            <(printf '%s\n' "$actual_members") >&2 || true
        exit 1
    fi

    for index in 0 1 2 3 4 5 6; do
        member_mode="$(
            tar -tvzf "$archive_path" "${archive_members[$index]}" |
                awk 'NR == 1 { print substr($1, 1, 10) }'
        )"
        if ((index == 0)); then
            [[ "$member_mode" == "-rwxr-xr-x" ]] ||
                fail "archive binary mode is $member_mode, expected -rwxr-xr-x"
        else
            [[ "$member_mode" == "-rw-r--r--" ]] ||
                fail "${archive_members[$index]} mode is $member_mode, expected -rw-r--r--"
        fi
    done

    tar -xzf "$archive_path" -C "$temporary_directory"

    for index in 0 1 2 3 4 5 6; do
        extracted_file="${temporary_directory}/${archive_members[$index]}"
        [[ -f "$extracted_file" && ! -L "$extracted_file" ]] ||
            fail "archive member is not a regular file: ${archive_members[$index]}"
        cmp -s "$extracted_file" "${source_files[$index]}" ||
            fail "archive member differs from its source: ${archive_members[$index]}"
    done

    extracted_binary="${temporary_directory}/viewr"
    [[ -x "$extracted_binary" ]] || fail "archive binary is not executable"
    elf_header="$(readelf -h "$extracted_binary")"
    grep -Fq 'Class:                             ELF64' <<<"$elf_header" ||
        fail "archive binary is not ELF64"
    grep -Fq 'Data:                              2'\''s complement, little endian' \
        <<<"$elf_header" ||
        fail "archive binary is not little-endian"
    grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' \
        <<<"$elf_header" ||
        fail "archive binary is not x86-64"
fi

printf 'Validated %s\n' "$archive_path"
