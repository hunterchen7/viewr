#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/test-macos-pkg-install.sh \
  --allow-system-changes VIEWR_PKG

Install and remove Viewr on a disposable macOS host. The test refuses to
replace an existing app, package receipt, or Launch Services registration.
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

if [[ "${1:-}" != "--allow-system-changes" || $# -ne 2 ]]; then
    usage >&2
    exit 2
fi

[[ "$(uname -s)" == "Darwin" ]] ||
    fail "the macOS install test must run on macOS"

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
pkg_arg="$2"
[[ -f "$pkg_arg" && -s "$pkg_arg" ]] ||
    fail "installer package is missing or empty: $pkg_arg"
pkg_dir="$(cd "$(dirname "$pkg_arg")" && pwd)"
pkg="$pkg_dir/$(basename "$pkg_arg")"

for command in \
    awk basename codesign diff dirname ditto find grep mktemp plutil \
    sed sort stat sudo uname xcrun
do
    command -v "$command" >/dev/null ||
        fail "required command is unavailable: $command"
done
[[ -x /usr/sbin/installer ]] ||
    fail "macOS installer command is unavailable"
pkgutil="/usr/sbin/pkgutil"
[[ -x "$pkgutil" ]] || fail "macOS package utility is unavailable"
[[ -x /usr/bin/defaults ]] || fail "macOS defaults command is unavailable"

app="/Applications/Viewr.app"
receipt="com.hunterchen.viewr.pkg"
bundle_identifier="com.hunterchen.viewr"
arw_type="com.sony.arw-raw-image"
launch_services="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
[[ -x "$launch_services" ]] ||
    fail "Launch Services registration tool is unavailable"

workspace_version="$(
    awk -F'"' '/^version = "/ { print $2; exit }' "$repo_root/Cargo.toml"
)"
[[ "$workspace_version" =~ ^[0-9]+(\.[0-9]+){2}$ ]] ||
    fail "workspace version is not a three-part numeric version"

if [[ -e "$app" || -L "$app" ]]; then
    fail "refusing to replace an existing $app"
fi
if "$pkgutil" --pkg-info "$receipt" >/dev/null 2>&1; then
    fail "refusing to replace an existing $receipt package receipt"
fi
"$repo_root/scripts/validate-macos-pkg.sh" "$pkg"
sudo -n true

temp_base="${TMPDIR:-/tmp}"
work_dir="$(mktemp -d "${temp_base%/}/viewr-macos-install.XXXXXX")"
work_dir="$(cd "$work_dir" && pwd -P)"
install_attempted=0
test_completed=0

launch_services_preferences() {
    local error_file="$work_dir/defaults-read-error.txt"
    local preferences
    if preferences="$(
        LC_ALL=C /usr/bin/defaults read \
            com.apple.LaunchServices/com.apple.launchservices.secure \
            LSHandlers 2>"$error_file"
    )"; then
        /bin/rm -f -- "$error_file"
        printf '%s\n' "$preferences"
    elif grep -Fq 'does not exist' "$error_file"; then
        /bin/rm -f -- "$error_file"
        printf '<absent>\n'
    else
        /bin/cat "$error_file" >&2
        echo "error: could not read Launch Services handler preferences" >&2
        return 1
    fi
}

cleanup() {
    original_status=$?
    trap - EXIT
    trap '' INT TERM
    cleanup_failed=0

    if [[ "$install_attempted" == "1" ]]; then
        if [[ -e "$app" || -L "$app" ]]; then
            if ! "$launch_services" -u "$app" >/dev/null 2>&1; then
                echo "error: cleanup could not unregister $app" >&2
                cleanup_failed=1
            fi
            sudo -n /bin/rm -rf -- "$app" || cleanup_failed=1
        fi
        if "$pkgutil" --pkg-info "$receipt" >/dev/null 2>&1; then
            sudo -n "$pkgutil" --forget "$receipt" >/dev/null ||
                cleanup_failed=1
        fi
        if [[ -e "$app" || -L "$app" ]]; then
            echo "error: cleanup left $app behind" >&2
            cleanup_failed=1
        fi
        if "$pkgutil" --pkg-info "$receipt" >/dev/null 2>&1; then
            echo "error: cleanup left the $receipt receipt behind" >&2
            cleanup_failed=1
        fi

        launch_services_absent=0
        for _ in {1..300}; do
            if "$probe" absent \
                "$bundle_identifier" \
                "$arw_type" \
                "$app" >/dev/null 2>&1; then
                launch_services_absent=1
                break
            fi
            /bin/sleep 0.1
        done
        if [[ "$launch_services_absent" != "1" ]]; then
            echo "error: cleanup left a Viewr Launch Services registration" >&2
            cleanup_failed=1
        fi
        default_after_cleanup="$(
            "$probe" default \
                "$bundle_identifier" \
                "$arw_type" \
                "$app"
        )" || cleanup_failed=1
        if [[ -n "$default_before" &&
            "${default_after_cleanup:-}" != "$default_before" ]]; then
            echo "error: cleanup changed the default ARW application" >&2
            cleanup_failed=1
        fi
        preferences_after_cleanup="$(
            launch_services_preferences
        )" || cleanup_failed=1
        if [[ "${preferences_after_cleanup:-}" != "$preferences_before" ]]; then
            echo "error: cleanup changed Launch Services handler preferences" >&2
            cleanup_failed=1
        fi
    fi

    /bin/rm -rf -- "$work_dir" || cleanup_failed=1

    if [[ "$cleanup_failed" == "1" && "$original_status" == "0" ]]; then
        original_status=1
    fi
    if [[ "$original_status" == "0" && "$test_completed" == "1" ]]; then
        echo "macOS package install/removal integration test passed."
    fi
    exit "$original_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

probe="$work_dir/installed-app-probe"
xcrun --sdk macosx swiftc \
    -module-cache-path "$work_dir/swift-module-cache" \
    -sdk "$(xcrun --sdk macosx --show-sdk-path)" \
    -O \
    "$repo_root/packaging/macos/InstalledAppProbe.swift" \
    -o "$probe"

"$probe" absent "$bundle_identifier" "$arw_type" "$app" ||
    fail "refusing to replace an existing Launch Services registration"
default_before="$(
    "$probe" default "$bundle_identifier" "$arw_type" "$app"
)"
preferences_before="$(launch_services_preferences)"

expanded="$work_dir/expanded"
"$pkgutil" --expand "$pkg" "$expanded"
payload="$(find "$expanded" -type f -name Payload -print -quit)"
[[ -n "$payload" ]] || fail "expanded installer contains no component payload"
expected_root="$work_dir/expected"
/bin/mkdir -p "$expected_root"
ditto -x "$payload" "$expected_root"
expected_app="$expected_root/Viewr.app"
[[ -d "$expected_app" && ! -L "$expected_app" ]] ||
    fail "installer payload does not contain a regular Viewr.app"

expected_receipt_files="$work_dir/expected-receipt-files.txt"
actual_receipt_files="$work_dir/actual-receipt-files.txt"
"$pkgutil" --payload-files "$pkg" |
    sed -e 's#^\./##' -e '/^\.$/d' -e 's#/$##' |
    LC_ALL=C sort >"$expected_receipt_files"

install_log="$work_dir/installer.log"
install_attempted=1
if ! sudo -n /usr/sbin/installer \
    -pkg "$pkg" \
    -target / \
    -verboseR >"$install_log" 2>&1; then
    /bin/cat "$install_log" >&2
    fail "macOS Installer rejected the Viewr package"
fi

[[ -d "$app" && ! -L "$app" ]] ||
    fail "macOS Installer did not create a regular $app"
"$pkgutil" --files "$receipt" |
    sed -e 's#^\./##' -e '/^\.$/d' -e 's#/$##' |
    LC_ALL=C sort >"$actual_receipt_files"
/usr/bin/cmp -s "$expected_receipt_files" "$actual_receipt_files" ||
    fail "installed package receipt does not own the exact payload"
diff -qr "$expected_app" "$app" >/dev/null ||
    fail "installed Viewr.app differs from the validated package payload"

receipt_plist="$work_dir/receipt.plist"
"$pkgutil" --pkg-info-plist "$receipt" >"$receipt_plist"
receipt_value() {
    plutil -extract "$1" raw -o - "$receipt_plist"
}
[[ "$(receipt_value pkgid)" == "$receipt" ]] ||
    fail "installed receipt has an unexpected package identifier"
[[ "$(receipt_value pkg-version)" == "$workspace_version" ]] ||
    fail "installed receipt has an unexpected version"
[[ "$(receipt_value volume)" == "/" ]] ||
    fail "installed receipt has an unexpected target volume"
receipt_location="$(receipt_value install-location)"
[[ "/${receipt_location#/}" == "/Applications" ]] ||
    fail "installed receipt has an unexpected install location"

unexpected_owner="$(
    find "$app" \( ! -user root -o ! -group wheel \) -print
)"
[[ -z "$unexpected_owner" ]] ||
    fail "installed payload is not owned by root:wheel"
unexpected_writable="$(
    find "$app" \( -perm -0002 -o -perm -0020 \) -print
)"
[[ -z "$unexpected_writable" ]] ||
    fail "installed payload is writable by group or other"
[[ "$(stat -f '%Lp' "$app/Contents/MacOS/ViewrLauncher")" == "755" ]] ||
    fail "installed launcher mode is not 0755"
[[ "$(stat -f '%Lp' "$app/Contents/MacOS/viewr-bin")" == "755" ]] ||
    fail "installed viewer mode is not 0755"

info="$app/Contents/Info.plist"
installed_plist_value() {
    plutil -extract "$1" raw -o - "$info"
}
[[ "$(installed_plist_value CFBundleIdentifier)" == "$bundle_identifier" ]] ||
    fail "installed app has an unexpected bundle identifier"
[[ "$(installed_plist_value CFBundleExecutable)" == "ViewrLauncher" ]] ||
    fail "installed app has an unexpected executable"
[[ "$(installed_plist_value CFBundleVersion)" == "$workspace_version" ]] ||
    fail "installed app has an unexpected version"
codesign --verify --deep --strict --verbose=2 "$app"

usage_output="$("$app/Contents/MacOS/viewr-bin" 2>&1)" ||
    fail "installed viewer usage smoke test failed"
grep -Fq 'usage: viewr <folder|file.arw>' <<<"$usage_output" ||
    fail "installed viewer did not print its usage text"

launch_services_ready=0
for _ in {1..300}; do
    if "$probe" present \
        "$bundle_identifier" \
        "$arw_type" \
        "$app" >/dev/null 2>&1; then
        launch_services_ready=1
        break
    fi
    /bin/sleep 0.1
done
[[ "$launch_services_ready" == "1" ]] ||
    fail "Installer did not register Viewr and its ARW handler with Launch Services"
default_after_install="$(
    "$probe" default "$bundle_identifier" "$arw_type" "$app"
)"
if [[ -n "$default_before" ]]; then
    [[ "$default_after_install" == "$default_before" ]] ||
        fail "Installer changed the existing default ARW application"
fi
preferences_after_install="$(launch_services_preferences)"
[[ "$preferences_after_install" == "$preferences_before" ]] ||
    fail "Installer changed Launch Services handler preferences"

test_completed=1
