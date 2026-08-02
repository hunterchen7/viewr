#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/test-macos-pkg-install.sh \
  --allow-system-changes VIEWR_PKG

Install and remove Viewr on a disposable Apple Silicon Mac with macOS 12 or
later. The test refuses to replace an existing app, package receipt, or Launch
Services registration. Set VIEWR_TEST_RAW to an existing Sony ARW file. The
test does not change it.
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
    sed shasum sort stat sudo uname xcrun
do
    command -v "$command" >/dev/null ||
        fail "required command is unavailable: $command"
done
[[ -x /usr/sbin/installer ]] ||
    fail "macOS installer command is unavailable"
pkgutil="/usr/sbin/pkgutil"
[[ -x "$pkgutil" ]] || fail "macOS package utility is unavailable"
[[ -x /usr/bin/defaults ]] || fail "macOS defaults command is unavailable"
[[ -x /usr/bin/sw_vers ]] || fail "macOS version utility is unavailable"
macos_major="$(/usr/bin/sw_vers -productVersion | awk -F. '{ print $1 }')"
[[ "$macos_major" =~ ^[0-9]+$ && "$macos_major" -ge 12 ]] ||
    fail "the macOS install test requires macOS 12 or later"

app="/Applications/Viewr.app"
recovery_bundle="/Applications/.Viewr-system-recovery.app"
receipt="com.hunterchen.viewr.pkg"
bundle_identifier="com.hunterchen.viewr"
arw_type="com.sony.arw-raw-image"
launch_services="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
[[ -x "$launch_services" ]] ||
    fail "Launch Services registration tool is unavailable"

fixture_arg="${VIEWR_TEST_RAW:-}"
[[ -n "$fixture_arg" ]] ||
    fail "VIEWR_TEST_RAW must name an existing Sony ARW file"
[[ -f "$fixture_arg" && -s "$fixture_arg" && -r "$fixture_arg" ]] ||
    fail "ARW fixture is missing, empty, or unreadable: $fixture_arg"
case "${fixture_arg##*.}" in
    [Aa][Rr][Ww]) ;;
    *) fail "ARW fixture must have an .arw extension: $fixture_arg" ;;
esac
fixture_dir="$(cd "$(dirname "$fixture_arg")" && pwd -P)"
fixture="$fixture_dir/$(basename "$fixture_arg")"
case "$fixture" in
    "$app"|"$app"/*)
        fail "ARW fixture cannot be inside $app"
        ;;
esac
fixture_sha256="$(shasum -a 256 "$fixture" | awk '{ print $1 }')"

workspace_version="$(
    awk -F'"' '/^version = "/ { print $2; exit }' "$repo_root/Cargo.toml"
)"
[[ "$workspace_version" =~ ^[0-9]+(\.[0-9]+){2}$ ]] ||
    fail "workspace version is not a three-part numeric version"

if [[ -e "$app" || -L "$app" ]]; then
    fail "refusing to replace an existing $app"
fi
if [[ -e "$recovery_bundle" || -L "$recovery_bundle" ]]; then
    fail "refusing to replace an existing $recovery_bundle"
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
recovery_fixture_created=0
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

    if [[ "$recovery_fixture_created" == "1" &&
        ( -e "$recovery_bundle" || -L "$recovery_bundle" ) ]]; then
        sudo -n /bin/rm -rf -- "$recovery_bundle" || cleanup_failed=1
    fi

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
                "$app" \
                "$fixture" >/dev/null 2>&1; then
                launch_services_absent=1
                break
            fi
            /bin/sleep 0.1
        done
        if [[ "$launch_services_absent" != "1" ]]; then
            "$probe" absent \
                "$bundle_identifier" \
                "$arw_type" \
                "$app" \
                "$fixture" || true
            echo "error: cleanup left a Viewr Launch Services registration" >&2
            cleanup_failed=1
        fi
        default_after_cleanup="$(
            "$probe" default \
                "$bundle_identifier" \
                "$arw_type" \
                "$app" \
                "$fixture"
        )" || cleanup_failed=1
        type_default_after_cleanup="$(
            "$probe" type-default \
                "$bundle_identifier" \
                "$arw_type" \
                "$app" \
                "$fixture"
        )" || cleanup_failed=1
        # Launch Services may choose a different temporary Viewr build as its
        # implicit default after the installed app is unregistered. An explicit
        # user selection must remain exact; the preferences comparison below
        # separately proves that the installer did not write a new selection.
        if [[ "$explicit_binding_before" == "present" &&
            ( "${default_after_cleanup:-}" != "$default_before" ||
                "${type_default_after_cleanup:-}" != "$type_default_before" ) ]]; then
            echo "error: cleanup changed the default ARW application" >&2
            echo "file default before: ${default_before:-<none>}" >&2
            echo "file default after: ${default_after_cleanup:-<none>}" >&2
            echo "UTI default before: ${type_default_before:-<none>}" >&2
            echo "UTI default after: ${type_default_after_cleanup:-<none>}" >&2
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

    if [[ ! -f "$fixture" ||
        "$(shasum -a 256 "$fixture" | awk '{ print $1 }')" != "$fixture_sha256" ]]; then
        echo "error: integration test changed the ARW fixture" >&2
        cleanup_failed=1
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
    -warnings-as-errors \
    -target arm64-apple-macosx12.0 \
    -module-cache-path "$work_dir/swift-module-cache" \
    -sdk "$(xcrun --sdk macosx --show-sdk-path)" \
    -O \
    "$repo_root/packaging/macos/InstalledAppProbe.swift" \
    -o "$probe"

"$probe" absent "$bundle_identifier" "$arw_type" "$app" "$fixture" ||
    fail "refusing to replace an existing Launch Services registration"
default_before="$(
    "$probe" default "$bundle_identifier" "$arw_type" "$app" "$fixture"
)"
type_default_before="$(
    "$probe" type-default "$bundle_identifier" "$arw_type" "$app" "$fixture"
)"
explicit_binding_before="$(
    "$probe" explicit-binding \
        "$bundle_identifier" \
        "$arw_type" \
        "$app" \
        "$fixture"
)"
[[ "$explicit_binding_before" == "present" ||
    "$explicit_binding_before" == "absent" ]] ||
    fail "could not classify the existing ARW handler preference"
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
sudo -n /bin/mkdir -p "$recovery_bundle/Contents"
sudo -n /usr/bin/touch "$recovery_bundle/Contents/old-package-recovery"
recovery_fixture_created=1
install_attempted=1
if ! sudo -n /usr/sbin/installer \
    -pkg "$pkg" \
    -target / \
    -verboseR >"$install_log" 2>&1; then
    /bin/cat "$install_log" >&2
    fail "macOS Installer rejected the Viewr package"
fi

if [[ ! -d "$app" || -L "$app" ]]; then
    /bin/cat "$install_log" >&2
    fail "macOS Installer did not create a regular $app"
fi
[[ ! -e "$recovery_bundle" && ! -L "$recovery_bundle" ]] ||
    fail "macOS Installer did not remove the retained recovery bundle"
recovery_fixture_created=0
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
        "$app" \
        "$fixture" >/dev/null 2>&1; then
        launch_services_ready=1
        break
    fi
    /bin/sleep 0.1
done
if [[ "$launch_services_ready" != "1" ]]; then
    "$probe" present \
        "$bundle_identifier" \
        "$arw_type" \
        "$app" \
        "$fixture" || true
    fail "Installer did not register Viewr and its ARW handler with Launch Services"
fi
default_after_install="$(
    "$probe" default "$bundle_identifier" "$arw_type" "$app" "$fixture"
)"
type_default_after_install="$(
    "$probe" type-default "$bundle_identifier" "$arw_type" "$app" "$fixture"
)"
preferences_after_install="$(launch_services_preferences)"
[[ "$preferences_after_install" == "$preferences_before" ]] ||
    fail "Installer changed Launch Services handler preferences"
# Preview claims the generic camera-RAW parent type. On an account without an
# explicit ARW binding, Launch Services can prefer Viewr's exact child-type
# claim while it is registered even though Alternate is the lowest opener rank.
if [[ "$default_after_install" != "$default_before" ||
    "$type_default_after_install" != "$type_default_before" ]]; then
    if [[ "$explicit_binding_before" == "present" ]]; then
        echo "error: Installer displaced an explicit ARW default" >&2
        echo "file default before: ${default_before:-<none>}" >&2
        echo "file default after: ${default_after_install:-<none>}" >&2
        echo "UTI default before: ${type_default_before:-<none>}" >&2
        echo "UTI default after: ${type_default_after_install:-<none>}" >&2
        exit 1
    fi
    if [[ "$default_before" != "/System/Applications/Preview.app" ||
        "$type_default_before" != "/System/Applications/Preview.app" ||
        "$default_after_install" != "$app" ||
        "$type_default_after_install" != "$app" ]]; then
        echo "error: Installer caused an unexpected ARW default transition" >&2
        echo "file default before: ${default_before:-<none>}" >&2
        echo "file default after: ${default_after_install:-<none>}" >&2
        echo "UTI default before: ${type_default_before:-<none>}" >&2
        echo "UTI default after: ${type_default_after_install:-<none>}" >&2
        exit 1
    fi
    echo "Launch Services recomputed its implicit ARW default." >&2
    echo "Explicit handler preferences remain unchanged." >&2
    echo "file default before: ${default_before:-<none>}" >&2
    echo "file default after: ${default_after_install:-<none>}" >&2
    echo "UTI default before: ${type_default_before:-<none>}" >&2
    echo "UTI default after: ${type_default_after_install:-<none>}" >&2
fi

test_completed=1
