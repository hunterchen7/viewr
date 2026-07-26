#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/validate-macos-pkg.sh [--test-open-events] VIEWR_PKG

Validate the Viewr macOS installer without installing it. The optional open
event test launches an isolated copy with a probe executable and verifies two
sequential Finder-style document opens reach the same live launcher.

Optional environment variables:
  VIEWR_VERSION                 Expected package version. Defaults to Cargo.toml.
  VIEWR_MACOS_REQUIRE_SIGNED    Set to 1 to reject an unsigned installer.
  RAWLER_LICENSE_PATH           Expected rawler 0.7.2 LICENSE. Defaults to the
                                 checked-in canonical copy. Its SHA-256 must match.
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

test_open_events=0
if [[ "${1:-}" == "--test-open-events" ]]; then
    test_open_events=1
    shift
fi
[[ $# -eq 1 ]] || {
    usage >&2
    exit 64
}

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
pkg_arg="$1"
[[ -f "$pkg_arg" ]] || fail "installer package does not exist: $pkg_arg"
pkg_dir="$(cd "$(dirname "$pkg_arg")" && pwd)"
pkg="$pkg_dir/$(basename "$pkg_arg")"

for command in codesign lipo pkgutil plutil shasum vtool xcrun xmllint; do
    command -v "$command" >/dev/null || fail "required command is unavailable: $command"
done

workspace_version="$(
    awk -F'"' '/^version = "/ { print $2; exit }' "$repo_root/Cargo.toml"
)"
expected_version="${VIEWR_VERSION:-$workspace_version}"

temp_base="${TMPDIR:-/tmp}"
work_dir="$(mktemp -d "${temp_base%/}/viewr-macos-validate.XXXXXX")"
work_dir="$(cd "$work_dir" && pwd -P)"
test_process_ids=""
test_app=""
test_app_opened=0
probe_log=""
cleanup() {
    original_status=$?
    trap - EXIT
    trap '' INT TERM
    cleanup_failed=0

    if [[ -n "$probe_log" && -f "$probe_log" ]]; then
        while IFS=$'\t' read -r process_id _; do
            if [[ "$process_id" =~ ^[0-9]+$ ]]; then
                test_process_ids="$test_process_ids $process_id"
            fi
        done <"$probe_log"
    fi
    if [[ -n "$test_app" ]]; then
        while IFS= read -r process_id; do
            if [[ "$process_id" =~ ^[0-9]+$ ]]; then
                test_process_ids="$test_process_ids $process_id"
            fi
        done < <(
            /bin/ps -axo pid=,command= |
                awk \
                    -v launcher="$test_app/Contents/MacOS/ViewrLauncher" \
                    -v viewer="$test_app/Contents/MacOS/viewr-bin" '
                        {
                            process_id = $1
                            sub(/^[[:space:]]*[0-9]+[[:space:]]+/, "")
                            if ($0 == launcher ||
                                $0 == viewer ||
                                index($0, viewer " ") == 1) {
                                print process_id
                            }
                        }
                    '
        )
    fi
    for process_id in $test_process_ids; do
        if [[ "$process_id" =~ ^[0-9]+$ ]]; then
            /bin/kill "$process_id" 2>/dev/null || true
        fi
    done
    for _ in {1..50}; do
        processes_alive=0
        for process_id in $test_process_ids; do
            if [[ "$process_id" =~ ^[0-9]+$ ]] &&
                /bin/kill -0 "$process_id" 2>/dev/null; then
                processes_alive=1
            fi
        done
        [[ "$processes_alive" == "0" ]] && break
        /bin/sleep 0.1
    done
    for process_id in $test_process_ids; do
        if [[ "$process_id" =~ ^[0-9]+$ ]] &&
            /bin/kill -0 "$process_id" 2>/dev/null; then
            /bin/kill -KILL "$process_id" 2>/dev/null || cleanup_failed=1
        fi
    done
    for _ in {1..50}; do
        processes_alive=0
        for process_id in $test_process_ids; do
            if [[ "$process_id" =~ ^[0-9]+$ ]] &&
                /bin/kill -0 "$process_id" 2>/dev/null; then
                processes_alive=1
            fi
        done
        [[ "$processes_alive" == "0" ]] && break
        /bin/sleep 0.1
    done
    if [[ "$processes_alive" != "0" ]]; then
        echo "error: macOS open-event test processes did not exit" >&2
        cleanup_failed=1
    fi

    if [[ "$test_app_opened" == "1" ]]; then
        launch_services="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
        if [[ ! -x "$launch_services" ]] ||
            ! "$launch_services" -u "$test_app" >/dev/null 2>&1; then
            echo "error: could not unregister the macOS open-event test app" >&2
            cleanup_failed=1
        fi
    fi
    if [[ "${VIEWR_MACOS_KEEP_VALIDATION_DIR:-0}" == "1" ]]; then
        echo "Kept macOS validation directory: $work_dir" >&2
    elif [[ -n "${work_dir:-}" && -d "$work_dir" ]]; then
        /bin/rm -rf -- "$work_dir" || cleanup_failed=1
    fi

    if [[ "$cleanup_failed" == "1" && "$original_status" == "0" ]]; then
        original_status=1
    fi
    exit "$original_status"
}
trap cleanup EXIT

signature_report="$work_dir/package-signature.txt"
if pkgutil --check-signature "$pkg" >"$signature_report" 2>&1; then
    package_is_signed=1
else
    package_is_signed=0
fi
if [[ "${VIEWR_MACOS_REQUIRE_SIGNED:-0}" == "1" && "$package_is_signed" != "1" ]]; then
    cat "$signature_report" >&2
    fail "installer is not signed by a trusted installer identity"
fi

payload_listing="$work_dir/payload-files.txt"
pkgutil --payload-files "$pkg" | sed 's#^\./##' | LC_ALL=C sort >"$payload_listing"
expected_payload="$(cat <<'EOF' | LC_ALL=C sort
.
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
actual_payload="$(cat "$payload_listing")"
[[ "$actual_payload" == "$expected_payload" ]] ||
    fail "installer payload does not match the exact expected layout"

expanded="$work_dir/expanded"
pkgutil --expand "$pkg" "$expanded"
distribution="$expanded/Distribution"
[[ -f "$distribution" ]] || fail "expanded installer contains no Distribution"
[[ "$(
    xmllint \
        --xpath \
        'string(/installer-gui-script/options/@hostArchitectures)' \
        "$distribution"
)" == "arm64" ]] ||
    fail "installer does not restrict installation to arm64 hosts"
[[ "$(
    xmllint \
        --xpath \
        'count(/installer-gui-script/volume-check/allowed-os-versions/os-version)' \
        "$distribution"
)" == "1" ]] ||
    fail "installer must declare exactly one minimum macOS requirement"
[[ "$(
    xmllint \
        --xpath \
        'string(/installer-gui-script/volume-check/allowed-os-versions/os-version/@min)' \
        "$distribution"
)" == "11.0" ]] ||
    fail "installer minimum host macOS version is not 11.0"
package_info="$(find "$expanded" -type f -name PackageInfo -print -quit)"
[[ -n "$package_info" ]] || fail "expanded installer contains no PackageInfo"
package_attribute() {
    xmllint --xpath "string(/pkg-info/@$1)" "$package_info"
}
[[ "$(package_attribute identifier)" == "com.hunterchen.viewr.pkg" ]] ||
    fail "unexpected installer package identifier"
[[ "$(package_attribute version)" == "$expected_version" ]] ||
    fail "unexpected installer package version"
[[ "$(package_attribute install-location)" == "/Applications" ]] ||
    fail "installer destination is not /Applications"
[[ "$(package_attribute relocatable)" == "false" ]] ||
    fail "installer component must not be relocatable"
[[ "$(
    xmllint \
        --xpath \
        'count(/pkg-info/upgrade-bundle/bundle[@id="com.hunterchen.viewr"])' \
        "$package_info"
)" == "1" ]] ||
    fail "installer does not declare the Viewr bundle upgrade"

payload="$(find "$expanded" -type f -name Payload -print -quit)"
[[ -n "$payload" ]] || fail "expanded installer contains no component payload"

extracted="$work_dir/extracted"
/bin/mkdir -p "$extracted"
/usr/bin/ditto -x "$payload" "$extracted"
app="$extracted/Viewr.app"
[[ -d "$app" ]] || fail "payload does not install Viewr.app at /Applications"

info="$app/Contents/Info.plist"
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
    "$info")" == \
    "arw" ]] ||
    fail "Sony ARW UTI does not declare the arw extension"
[[ "$(/usr/libexec/PlistBuddy -c \
    'Print :UTImportedTypeDeclarations:0:UTTypeTagSpecification:public.mime-type' \
    "$info")" == \
    "image/x-sony-arw" ]] ||
    fail "Sony ARW UTI does not declare image/x-sony-arw"

launcher="$app/Contents/MacOS/ViewrLauncher"
viewer="$app/Contents/MacOS/viewr-bin"
macos_files="$(
    find "$app/Contents/MacOS" -mindepth 1 -maxdepth 1 -type f -exec basename {} \; |
        LC_ALL=C sort
)"
expected_macos_files="$(printf 'ViewrLauncher\nviewr-bin')"
[[ "$macos_files" == "$expected_macos_files" ]] ||
    fail "bundle contains unexpected executable payload files"
for executable in "$launcher" "$viewer"; do
    [[ -x "$executable" ]] || fail "payload executable is not executable: $executable"
    [[ "$(lipo -archs "$executable")" == "arm64" ]] ||
        fail "payload executable is not arm64-only: $executable"
    minos="$(vtool -show-build "$executable" | awk '/minos/ { print $2; exit }')"
    [[ "$minos" == "11.0" ]] ||
        fail "payload executable requires macOS $minos instead of 11.0: $executable"
done
codesign --verify --deep --strict --verbose=2 "$app"

for resource in \
    "$app/Contents/Resources/LICENSE.txt" \
    "$app/Contents/Resources/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" \
    "$app/Contents/Resources/THIRD-PARTY-LICENSES.txt" \
    "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt" \
    "$app/Contents/Resources/SOURCE-BUILD.md" \
    "$app/Contents/Resources/rawler-LICENSE.txt"; do
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

if [[ "$test_open_events" == "1" ]]; then
    command -v open >/dev/null || fail "open command is unavailable"
    command -v pgrep >/dev/null || fail "pgrep command is unavailable"

    test_app="$work_dir/OpenEventValidation.app"
    /usr/bin/ditto "$app" "$test_app"
    test_identifier="com.hunterchen.viewr.validation.$$"
    /usr/libexec/PlistBuddy \
        -c "Set :CFBundleIdentifier $test_identifier" \
        "$test_app/Contents/Info.plist"

    probe="$test_app/Contents/MacOS/viewr-bin"
    probe_log="$work_dir/viewr-launcher-probe.log"
    /bin/rm -f -- "$probe_log"
    xcrun --sdk macosx swiftc \
        -module-cache-path "$work_dir/swift-module-cache" \
        -target arm64-apple-macos11.0 \
        -sdk "$(xcrun --sdk macosx --show-sdk-path)" \
        -O \
        "$repo_root/packaging/macos/ViewerProbe.swift" \
        -o "$probe"
    codesign --force --sign - --timestamp=none "$probe"
    codesign --force --sign - --timestamp=none "$test_app/Contents/MacOS/ViewrLauncher"
    codesign --force --sign - --timestamp=none "$test_app"

    first_file="$work_dir/First.ARW"
    second_file="$work_dir/Second.ARW"
    /usr/bin/touch "$first_file" "$second_file"

    test_app_opened=1
    /usr/bin/open -a "$test_app" "$first_file"
    for _ in {1..50}; do
        [[ -f "$probe_log" ]] && grep -F "$first_file" "$probe_log" >/dev/null && break
        /bin/sleep 0.1
    done
    [[ -f "$probe_log" ]] && grep -F "$first_file" "$probe_log" >/dev/null ||
        fail "first open-document event did not reach the viewer probe"
    grep -F $'\tregular\t' "$probe_log" >/dev/null ||
        fail "spawned viewer does not use the regular macOS activation policy"

    wrapper_pid="$(
        pgrep -f -x "$test_app/Contents/MacOS/ViewrLauncher" | head -1 || true
    )"
    [[ "$wrapper_pid" =~ ^[0-9]+$ ]] ||
        fail "launcher exited while the first viewer process was still running"
    test_process_ids="$test_process_ids $wrapper_pid"

    /usr/bin/open -a "$test_app" "$second_file"
    for _ in {1..50}; do
        grep -F "$second_file" "$probe_log" >/dev/null && break
        /bin/sleep 0.1
    done
    grep -F "$second_file" "$probe_log" >/dev/null ||
        fail "second open-document event did not reach the viewer probe"
    /bin/kill -0 "$wrapper_pid" ||
        fail "launcher did not remain alive across sequential open events"
    [[ "$(
        awk -F '\t' -v parent="$wrapper_pid" '$2 == parent { count++ } END { print count + 0 }' \
            "$probe_log"
    )" == "2" ]] ||
        fail "open-document events did not reach the same live launcher"

    while IFS=$'\t' read -r process_id _; do
        if [[ "$process_id" =~ ^[0-9]+$ ]]; then
            test_process_ids="$test_process_ids $process_id"
        fi
    done <"$probe_log"
    /bin/rm -f -- "$probe_log"
fi

if [[ "$package_is_signed" == "1" ]]; then
    echo "Validated signed installer: $pkg"
else
    echo "Validated unsigned installer: $pkg"
fi
