#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/validate-macos-pkg.sh [--test-open-events] VIEWR_PKG

Validate the Viewr macOS installer without installing it. The optional open
event test launches an isolated copy with a probe executable. It verifies that
a same-folder batch starts one viewer and that later document opens reach the
same live launcher.

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

for command in codesign pkgutil plutil xcrun xmllint; do
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
trap 'exit 130' INT
trap 'exit 143' TERM

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
        'count(/pkg-info/scripts/postinstall[@file="./postinstall"])' \
        "$package_info"
)" == "1" ]] ||
    fail "installer does not declare the recovery-cleanup postinstall script"
postinstall="$(dirname "$package_info")/Scripts/postinstall"
[[ -f "$postinstall" && ! -L "$postinstall" ]] ||
    fail "installer recovery-cleanup script is missing"
/usr/bin/cmp -s "$repo_root/packaging/macos/scripts/postinstall" "$postinstall" ||
    fail "installer recovery-cleanup script differs from the reviewed source"
[[ "$(/usr/bin/stat -f '%Lp' "$postinstall")" == "755" ]] ||
    fail "installer recovery-cleanup script mode is not 0755"
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
VIEWR_VERSION="$expected_version" "$repo_root/scripts/validate-macos-app.sh" "$app"

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
    canonical_test_path() {
        local directory
        directory="$(cd "$(dirname "$1")" && pwd -P)" || return
        printf '%s/%s\n' "$directory" "$(basename "$1")"
    }
    probe_log_has_path() {
        local expected_path
        local logged_path
        expected_path="$(canonical_test_path "$1")" || return 2
        [[ -f "$probe_log" ]] || return 1
        while IFS=$'\t' read -r _ _ _ logged_path; do
            if [[ -n "$logged_path" &&
                "$(canonical_test_path "$logged_path" 2>/dev/null)" == "$expected_path" ]]; then
                return 0
            fi
        done <"$probe_log"
        return 1
    }
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
    alias_dir="$work_dir/Alias"
    third_dir="$work_dir/Other"
    /bin/mkdir "$alias_dir" "$third_dir"
    alias_file="$alias_dir/Alias.ARW"
    third_file="$third_dir/Third.ARW"
    /usr/bin/touch "$first_file" "$second_file" "$third_file"
    /bin/ln -s "$first_file" "$alias_file"

    test_app_opened=1
    /usr/bin/open -a "$test_app" "$alias_file" "$second_file"
    for _ in {1..50}; do
        probe_log_has_path "$first_file" && break
        /bin/sleep 0.1
    done
    if ! probe_log_has_path "$first_file"; then
        [[ ! -f "$probe_log" ]] || /bin/cat "$probe_log" >&2
        fail "first open-document event did not reach the viewer probe"
    fi
    grep -F $'\tregular\t' "$probe_log" >/dev/null ||
        fail "spawned viewer does not use the regular macOS activation policy"
    /bin/sleep 0.5
    [[ "$(/usr/bin/wc -l <"$probe_log" | /usr/bin/tr -d '[:space:]')" == "1" ]] ||
        fail "one same-folder open event launched more than one viewer"
    if probe_log_has_path "$second_file"; then
        fail "same-folder open event was not coalesced"
    fi

    wrapper_pid="$(
        pgrep -f -x "$test_app/Contents/MacOS/ViewrLauncher" | head -1 || true
    )"
    [[ "$wrapper_pid" =~ ^[0-9]+$ ]] ||
        fail "launcher exited while the first viewer process was still running"
    test_process_ids="$test_process_ids $wrapper_pid"

    /usr/bin/open -a "$test_app" "$second_file"
    for _ in {1..50}; do
        probe_log_has_path "$second_file" && break
        /bin/sleep 0.1
    done
    probe_log_has_path "$second_file" ||
        fail "later same-folder open event did not reach the viewer probe"

    /usr/bin/open -a "$test_app" "$third_file"
    for _ in {1..50}; do
        probe_log_has_path "$third_file" && break
        /bin/sleep 0.1
    done
    probe_log_has_path "$third_file" ||
        fail "different-folder open event did not reach the viewer probe"
    /bin/kill -0 "$wrapper_pid" ||
        fail "launcher did not remain alive across sequential open events"
    [[ "$(
        awk -F '\t' -v parent="$wrapper_pid" '$2 == parent { count++ } END { print count + 0 }' \
            "$probe_log"
    )" == "3" ]] ||
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
