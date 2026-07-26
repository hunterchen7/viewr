#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/package-macos-pkg.sh VIEWR_BINARY OUTPUT_PKG

Build an arm64 Viewr.app and a macOS installer package.

Optional environment variables:
  VIEWR_VERSION                         Override the workspace package version.
  VIEWR_RELEASE_TAG                     Assert that this tag is v<version>.
  VIEWR_MACOS_APP_SIGN_IDENTITY         Sign app executables and bundle with
                                         this codesign identity. The default is
                                         an ad-hoc signature.
  VIEWR_MACOS_INSTALLER_SIGN_IDENTITY   Sign the product archive with this
                                         installer identity. The default
                                         package is unsigned.
  RAWLER_LICENSE_PATH                   Use this rawler 0.7.2 LICENSE file
                                         instead of the checked-in canonical
                                         copy. Its SHA-256 must match.

This script does not notarize or staple the package.
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
[[ -f "$binary_arg" ]] || fail "viewer binary does not exist: $binary_arg"
binary_dir="$(cd "$(dirname "$binary_arg")" && pwd)"
viewer_binary="$binary_dir/$(basename "$binary_arg")"
product_requirements="$repo_root/packaging/macos/ProductRequirements.plist"

output_arg="$2"
/bin/mkdir -p "$(dirname "$output_arg")"
output_dir="$(cd "$(dirname "$output_arg")" && pwd)"
output_pkg="$output_dir/$(basename "$output_arg")"

for command in codesign lipo pkgbuild plutil productbuild shasum vtool xattr xcrun; do
    command -v "$command" >/dev/null || fail "required command is unavailable: $command"
done
[[ -f "$product_requirements" ]] ||
    fail "product requirements do not exist: $product_requirements"
plutil -lint "$product_requirements" >/dev/null

architectures="$(lipo -archs "$viewer_binary")"
[[ "$architectures" == "arm64" ]] ||
    fail "viewer binary must contain only arm64 code (found: $architectures)"

workspace_version="$(
    awk -F'"' '/^version = "/ { print $2; exit }' "$repo_root/Cargo.toml"
)"
version="${VIEWR_VERSION:-$workspace_version}"
[[ "$version" =~ ^[0-9]+(\.[0-9]+){1,2}$ ]] ||
    fail "installer version must contain two or three numeric components: $version"
if [[ -n "${VIEWR_RELEASE_TAG:-}" && "$VIEWR_RELEASE_TAG" != "v$version" ]]; then
    fail "release tag $VIEWR_RELEASE_TAG does not match workspace version v$version"
fi

temp_base="${TMPDIR:-/tmp}"
work_dir="$(mktemp -d "${temp_base%/}/viewr-macos-package.XXXXXX")"
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

launcher_minos="$(vtool -show-build "$macos_dir/ViewrLauncher" | awk '/minos/ { print $2; exit }')"
[[ "$launcher_minos" == "11.0" ]] ||
    fail "launcher minimum macOS version is $launcher_minos, expected 11.0"

component_pkg="$work_dir/Viewr-component.pkg"
pkgbuild \
    --component "$app" \
    --install-location /Applications \
    --identifier com.hunterchen.viewr.pkg \
    --version "$version" \
    "$component_pkg"

if [[ -e "$output_pkg" ]]; then
    /bin/rm -f -- "$output_pkg"
fi

if [[ -n "${VIEWR_MACOS_INSTALLER_SIGN_IDENTITY:-}" ]]; then
    productbuild \
        --product "$product_requirements" \
        --package "$component_pkg" \
        --sign "$VIEWR_MACOS_INSTALLER_SIGN_IDENTITY" \
        "$output_pkg"
else
    productbuild \
        --product "$product_requirements" \
        --package "$component_pkg" \
        "$output_pkg"
fi

echo "Created $output_pkg"
