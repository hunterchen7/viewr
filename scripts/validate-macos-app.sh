#!/bin/bash
set -euo pipefail

export LC_ALL=C

usage() {
    cat <<'EOF'
Usage: scripts/validate-macos-app.sh VIEWR_APP

Validate the exact structure, metadata, resources, code, and signature of a
Viewr.app bundle.

Optional environment variables:
  VIEWR_VERSION          Expected app version. Defaults to Cargo.toml.
  RAWLER_LICENSE_PATH    Expected rawler 0.7.2 LICENSE. Defaults to the
                          checked-in canonical copy. Its SHA-256 must match.
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ $# -eq 1 ]] || {
    usage >&2
    exit 64
}

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

app_arg="$1"
[[ -d "$app_arg" && ! -L "$app_arg" ]] ||
    fail "app is not a regular directory: $app_arg"
[[ "$(basename "$app_arg")" == "Viewr.app" ]] ||
    fail "app must be named Viewr.app"
app_dir="$(cd "$(dirname "$app_arg")" && pwd)"
app="$app_dir/Viewr.app"

for command in awk basename cat cmp codesign find grep lipo plutil shasum sort stat vtool; do
    command -v "$command" >/dev/null || fail "required command is unavailable: $command"
done

workspace_version="$(
    awk -F'"' '/^version = "/ { print $2; exit }' "$repo_root/Cargo.toml"
)"
expected_version="${VIEWR_VERSION:-$workspace_version}"

expected_layout="$(cat <<'EOF' | sort
Viewr.app
Viewr.app/Contents
Viewr.app/Contents/Info.plist
Viewr.app/Contents/MacOS
Viewr.app/Contents/MacOS/ViewrLauncher
Viewr.app/Contents/MacOS/viewr-bin
Viewr.app/Contents/PkgInfo
Viewr.app/Contents/Resources
Viewr.app/Contents/Resources/LICENSE.txt
Viewr.app/Contents/Resources/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html
Viewr.app/Contents/Resources/SOURCE-BUILD.md
Viewr.app/Contents/Resources/THIRD-PARTY-LICENSES.txt
Viewr.app/Contents/Resources/THIRD-PARTY-NOTICES.txt
Viewr.app/Contents/Resources/rawler-LICENSE.txt
Viewr.app/Contents/_CodeSignature
Viewr.app/Contents/_CodeSignature/CodeResources
EOF
)"
actual_layout="$(
    cd "$app_dir"
    find Viewr.app -print | sort
)"
[[ "$actual_layout" == "$expected_layout" ]] ||
    fail "app does not match the exact expected layout"
[[ -z "$(find "$app" -type l -print -quit)" ]] ||
    fail "app contains a symbolic link"
[[ -z "$(find "$app" ! -type d ! -type f -print -quit)" ]] ||
    fail "app contains a special file"

expected_directories=(
    "$app"
    "$app/Contents"
    "$app/Contents/MacOS"
    "$app/Contents/Resources"
    "$app/Contents/_CodeSignature"
)
for directory in "${expected_directories[@]}"; do
    [[ "$(stat -f '%Lp' "$directory")" == "755" ]] ||
        fail "app directory mode is not 0755: $directory"
done

launcher="$app/Contents/MacOS/ViewrLauncher"
viewer="$app/Contents/MacOS/viewr-bin"
for executable in "$launcher" "$viewer"; do
    [[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] ||
        fail "app executable is not a regular executable file: $executable"
    [[ "$(stat -f '%Lp' "$executable")" == "755" ]] ||
        fail "app executable mode is not 0755: $executable"
    [[ "$(lipo -archs "$executable")" == "arm64" ]] ||
        fail "app executable is not arm64-only: $executable"
    minos="$(vtool -show-build "$executable" | awk '/minos/ { print $2; exit }')"
    [[ "$minos" == "11.0" ]] ||
        fail "app executable requires macOS $minos instead of 11.0: $executable"
done

expected_data_files=(
    "$app/Contents/Info.plist"
    "$app/Contents/PkgInfo"
    "$app/Contents/Resources/LICENSE.txt"
    "$app/Contents/Resources/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
    "$app/Contents/Resources/SOURCE-BUILD.md"
    "$app/Contents/Resources/THIRD-PARTY-LICENSES.txt"
    "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt"
    "$app/Contents/Resources/rawler-LICENSE.txt"
    "$app/Contents/_CodeSignature/CodeResources"
)
for data_file in "${expected_data_files[@]}"; do
    [[ -f "$data_file" && ! -L "$data_file" ]] ||
        fail "app data file is not a regular file: $data_file"
    [[ "$(stat -f '%Lp' "$data_file")" == "644" ]] ||
        fail "app data file mode is not 0644: $data_file"
done

[[ "$(cat "$app/Contents/PkgInfo")" == "APPL????" ]] ||
    fail "app PkgInfo is invalid"

info="$app/Contents/Info.plist"
plutil -lint "$info" >/dev/null
plist_value() {
    plutil -extract "$1" raw -o - "$info"
}

[[ "$(plist_value CFBundleIdentifier)" == "com.hunterchen.viewr" ]] ||
    fail "unexpected bundle identifier"
[[ "$(plist_value CFBundleExecutable)" == "ViewrLauncher" ]] ||
    fail "unexpected bundle executable"
[[ "$(plist_value CFBundleShortVersionString)" == "$expected_version" ]] ||
    fail "unexpected short version"
[[ "$(plist_value CFBundleVersion)" == "$expected_version" ]] ||
    fail "unexpected bundle version"
[[ "$(plist_value LSMinimumSystemVersion)" == "11.0" ]] ||
    fail "bundle minimum macOS version is not 11.0"
if plutil -extract LSUIElement raw -o - "$info" >/dev/null 2>&1; then
    fail "bundle-wide LSUIElement would hide the spawned viewer from the Dock"
fi
[[ "$(plist_value CFBundleDocumentTypes.0.CFBundleTypeRole)" == "Viewer" ]] ||
    fail "ARW document role is not Viewer"
[[ "$(plist_value CFBundleDocumentTypes.0.LSHandlerRank)" == "Alternate" ]] ||
    fail "ARW handler rank is not Alternate"
[[ "$(plist_value CFBundleDocumentTypes.0.LSItemContentTypes.0)" == \
    "com.sony.arw-raw-image" ]] ||
    fail "document type does not use the exact Sony ARW UTI"
[[ "$(plist_value UTImportedTypeDeclarations.0.UTTypeIdentifier)" == \
    "com.sony.arw-raw-image" ]] ||
    fail "imported type does not declare the exact Sony ARW UTI"
[[ "$(plist_value UTImportedTypeDeclarations.0.UTTypeConformsTo.0)" == \
    "public.camera-raw-image" ]] ||
    fail "Sony ARW UTI does not conform to public.camera-raw-image"
[[ "$(/usr/libexec/PlistBuddy -c \
    'Print :UTImportedTypeDeclarations:0:UTTypeTagSpecification:public.filename-extension:0' \
    "$info")" == "arw" ]] ||
    fail "Sony ARW UTI does not declare the arw extension"
[[ "$(/usr/libexec/PlistBuddy -c \
    'Print :UTImportedTypeDeclarations:0:UTTypeTagSpecification:public.mime-type' \
    "$info")" == "image/x-sony-arw" ]] ||
    fail "Sony ARW UTI does not declare image/x-sony-arw"

codesign --verify --deep --strict --verbose=2 "$app"

for resource in "${expected_data_files[@]:2:6}"; do
    [[ -s "$resource" ]] || fail "license resource is empty: $resource"
done
/usr/bin/cmp -s "$repo_root/LICENSE" "$app/Contents/Resources/LICENSE.txt" ||
    fail "bundled Viewr LICENSE differs from the repository LICENSE"
/usr/bin/cmp -s \
    "$repo_root/packaging/THIRD-PARTY-LICENSES.txt" \
    "$app/Contents/Resources/THIRD-PARTY-LICENSES.txt" ||
    fail "bundled third-party licenses differ from the generated inventory"
/usr/bin/cmp -s \
    "$repo_root/packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" \
    "$app/Contents/Resources/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" ||
    fail "bundled Rust standard-library notices differ from the pinned copy"
/usr/bin/cmp -s \
    "$repo_root/packaging/THIRD-PARTY-NOTICES.txt" \
    "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt" ||
    fail "bundled third-party notice differs from the repository notice"
/usr/bin/cmp -s \
    "$repo_root/packaging/SOURCE-BUILD.md" \
    "$app/Contents/Resources/SOURCE-BUILD.md" ||
    fail "bundled source-build instructions differ from the repository copy"
grep -F "rawler 0.7.2" "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt" >/dev/null ||
    fail "third-party notice does not identify rawler 0.7.2"

rawler_license="${RAWLER_LICENSE_PATH:-$repo_root/packaging/licenses/rawler-0.7.2-LICENSE}"
[[ -f "$rawler_license" && -r "$rawler_license" ]] ||
    fail "rawler 0.7.2 LICENSE is not readable: $rawler_license"
expected_rawler_license_sha256="c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"
actual_rawler_license_sha256="$(shasum -a 256 "$rawler_license" | awk '{ print $1 }')"
[[ "$actual_rawler_license_sha256" == "$expected_rawler_license_sha256" ]] ||
    fail "rawler 0.7.2 LICENSE has unexpected SHA-256: $actual_rawler_license_sha256"
/usr/bin/cmp -s "$rawler_license" "$app/Contents/Resources/rawler-LICENSE.txt" ||
    fail "bundled rawler LICENSE differs from rawler 0.7.2"

echo "Validated $app"
