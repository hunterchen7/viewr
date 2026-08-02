#!/bin/bash
set -euo pipefail

export LC_ALL=C
umask 022

usage() {
    cat <<'EOF'
Usage: scripts/build-macos-app.sh VIEWR_BINARY OUTPUT_APP

Build and sign an arm64 Viewr.app bundle.

Optional environment variables:
  VIEWR_VERSION                    Override the workspace package version.
  VIEWR_RELEASE_TAG                Assert that this tag is v<version>.
  VIEWR_MACOS_APP_SIGN_IDENTITY    Sign the app with this codesign identity.
                                    The default is an ad-hoc signature.
  RAWLER_LICENSE_PATH              Use this rawler 0.7.2 LICENSE file instead
                                    of the checked-in canonical copy. Its
                                    SHA-256 must match.
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
repo_root="$(cd "$script_dir/.." && pwd)"

binary_arg="$1"
[[ -f "$binary_arg" && ! -L "$binary_arg" && -x "$binary_arg" ]] ||
    fail "viewer binary is not a regular executable file: $binary_arg"
binary_dir="$(cd "$(dirname "$binary_arg")" && pwd)"
viewer_binary="$binary_dir/$(basename "$binary_arg")"

output_arg="$2"
[[ "$(basename "$output_arg")" == "Viewr.app" ]] ||
    fail "output app must be named Viewr.app"
/bin/mkdir -p "$(dirname "$output_arg")"
output_dir="$(cd "$(dirname "$output_arg")" && pwd)"
output_app="$output_dir/Viewr.app"
[[ ! -e "$output_app" && ! -L "$output_app" ]] ||
    fail "output app already exists: $output_app"

for command in codesign lipo plutil shasum vtool xattr xcrun; do
    command -v "$command" >/dev/null || fail "required command is unavailable: $command"
done

architectures="$(lipo -archs "$viewer_binary")"
[[ "$architectures" == "arm64" ]] ||
    fail "viewer binary must contain only arm64 code (found: $architectures)"

workspace_version="$(
    awk -F'"' '/^version = "/ { print $2; exit }' "$repo_root/Cargo.toml"
)"
version="${VIEWR_VERSION:-$workspace_version}"
[[ "$version" =~ ^[0-9]+(\.[0-9]+){1,2}$ ]] ||
    fail "app version must contain two or three numeric components: $version"
if [[ -n "${VIEWR_RELEASE_TAG:-}" && "$VIEWR_RELEASE_TAG" != "v$version" ]]; then
    fail "release tag $VIEWR_RELEASE_TAG does not match workspace version v$version"
fi

work_dir="$(mktemp -d "$output_dir/.viewr-macos-app.XXXXXX")"
work_dir="$(cd "$work_dir" && pwd -P)"
cleanup() {
    if [[ -n "${work_dir:-}" && -d "$work_dir" ]]; then
        /bin/rm -rf -- "$work_dir"
    fi
}
trap cleanup EXIT

app="$work_dir/Viewr.app"
macos_dir="$app/Contents/MacOS"
resources_dir="$app/Contents/Resources"
/bin/mkdir -p "$macos_dir" "$resources_dir"

/bin/cp "$repo_root/packaging/macos/Info.plist.in" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy \
    -c "Set :CFBundleShortVersionString $version" \
    -c "Set :CFBundleVersion $version" \
    "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

sdk_path="$(xcrun --sdk macosx --show-sdk-path)"
xcrun --sdk macosx swiftc \
    -module-cache-path "$work_dir/swift-module-cache" \
    -target arm64-apple-macos11.0 \
    -sdk "$sdk_path" \
    -O \
    -whole-module-optimization \
    "$repo_root/packaging/macos/ViewrLauncher.swift" \
    -o "$macos_dir/ViewrLauncher"

/bin/cp "$viewer_binary" "$macos_dir/viewr-bin"
/bin/chmod 0755 "$macos_dir/ViewrLauncher" "$macos_dir/viewr-bin"
/usr/bin/printf 'APPL????' >"$app/Contents/PkgInfo"

/bin/cp "$repo_root/LICENSE" "$resources_dir/LICENSE.txt"
/bin/cp \
    "$repo_root/packaging/THIRD-PARTY-NOTICES.txt" \
    "$resources_dir/THIRD-PARTY-NOTICES.txt"
/bin/cp \
    "$repo_root/packaging/THIRD-PARTY-LICENSES.txt" \
    "$resources_dir/THIRD-PARTY-LICENSES.txt"
/bin/cp \
    "$repo_root/packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" \
    "$resources_dir/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
/bin/cp "$repo_root/packaging/SOURCE-BUILD.md" "$resources_dir/SOURCE-BUILD.md"

rawler_license="${RAWLER_LICENSE_PATH:-$repo_root/packaging/licenses/rawler-0.7.2-LICENSE}"
[[ -f "$rawler_license" && -r "$rawler_license" ]] ||
    fail "rawler 0.7.2 LICENSE is not readable: $rawler_license"
expected_rawler_license_sha256="c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"
actual_rawler_license_sha256="$(shasum -a 256 "$rawler_license" | awk '{ print $1 }')"
[[ "$actual_rawler_license_sha256" == "$expected_rawler_license_sha256" ]] ||
    fail "rawler 0.7.2 LICENSE has unexpected SHA-256: $actual_rawler_license_sha256"
/bin/cp "$rawler_license" "$resources_dir/rawler-LICENSE.txt"

/bin/chmod 0755 "$app" "$app/Contents" "$macos_dir" "$resources_dir"
/bin/chmod 0644 "$app/Contents/Info.plist" "$app/Contents/PkgInfo" "$resources_dir"/*
xattr -cr "$app"

app_identity="${VIEWR_MACOS_APP_SIGN_IDENTITY:--}"
if [[ "$app_identity" == "-" ]]; then
    codesign_args=(--force --sign - --timestamp=none)
else
    codesign_args=(--force --sign "$app_identity" --options runtime --timestamp)
fi

codesign "${codesign_args[@]}" "$macos_dir/viewr-bin"
codesign "${codesign_args[@]}" "$macos_dir/ViewrLauncher"
codesign "${codesign_args[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

for executable in "$macos_dir/ViewrLauncher" "$macos_dir/viewr-bin"; do
    minos="$(vtool -show-build "$executable" | awk '/minos/ { print $2; exit }')"
    [[ "$minos" == "11.0" ]] ||
        fail "executable minimum macOS version is $minos, expected 11.0: $executable"
done

/bin/mv "$app" "$output_app"
echo "Created $output_app"
