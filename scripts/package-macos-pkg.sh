#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/package-macos-pkg.sh VIEWR_BINARY OUTPUT_PKG
       scripts/package-macos-pkg.sh --app VIEWR_APP OUTPUT_PKG

Build a macOS installer from the reusable Viewr.app bundle.
Use --app to reuse an app that build-macos-app.sh already created.

Optional environment variables:
  VIEWR_VERSION                         Override the workspace package version.
  VIEWR_RELEASE_TAG                     Assert that this tag is v<version>.
  VIEWR_MACOS_APP_SIGN_IDENTITY         Sign app executables and bundle with
                                         this codesign identity. The default is
                                         an ad-hoc signature. This value applies
                                         only to the binary input form.
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

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

viewer_binary=""
input_app=""
if [[ $# -eq 3 && "$1" == "--app" ]]; then
    app_arg="$2"
    [[ -d "$app_arg" && ! -L "$app_arg" ]] ||
        fail "app is not a regular directory: $app_arg"
    app_dir="$(cd "$(dirname "$app_arg")" && pwd)"
    input_app="$app_dir/$(basename "$app_arg")"
    output_arg="$3"
elif [[ $# -eq 2 ]]; then
    binary_arg="$1"
    [[ -f "$binary_arg" && ! -L "$binary_arg" && -x "$binary_arg" ]] ||
        fail "viewer binary is not a regular executable file: $binary_arg"
    binary_dir="$(cd "$(dirname "$binary_arg")" && pwd)"
    viewer_binary="$binary_dir/$(basename "$binary_arg")"
    output_arg="$2"
else
    usage >&2
    exit 64
fi

product_requirements="$repo_root/packaging/macos/ProductRequirements.plist"
package_scripts="$repo_root/packaging/macos/scripts"

/bin/mkdir -p "$(dirname "$output_arg")"
output_dir="$(cd "$(dirname "$output_arg")" && pwd)"
output_pkg="$output_dir/$(basename "$output_arg")"

for command in pkgbuild plutil productbuild; do
    command -v "$command" >/dev/null || fail "required command is unavailable: $command"
done
[[ -f "$product_requirements" ]] ||
    fail "product requirements do not exist: $product_requirements"
[[ -x "$package_scripts/postinstall" && ! -L "$package_scripts/postinstall" ]] ||
    fail "installer postinstall script is missing or not executable"
plutil -lint "$product_requirements" >/dev/null

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

if [[ -n "$input_app" ]]; then
    app="$input_app"
    VIEWR_VERSION="$version" "$repo_root/scripts/validate-macos-app.sh" "$app"
else
    app="$work_dir/Viewr.app"
    "$repo_root/scripts/build-macos-app.sh" "$viewer_binary" "$app"
fi

component_pkg="$work_dir/Viewr-component.pkg"
pkgbuild \
    --component "$app" \
    --scripts "$package_scripts" \
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
