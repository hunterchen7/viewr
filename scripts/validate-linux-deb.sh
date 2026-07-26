#!/usr/bin/env bash
#
# Validate the Linux installer without installing it or changing host MIME
# associations. Intended for an Ubuntu release job after package assembly.

set -euo pipefail
export LC_ALL=C

usage() {
    printf 'Usage: scripts/validate-linux-deb.sh PATH-TO-DEB\n'
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

if (($# != 1)); then
    usage >&2
    exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/.." && pwd -P)"
package_path="$1"

for command_name in \
    awk cargo cmp desktop-file-validate dpkg-deb find grep gzip jq ldd lintian \
    md5sum mktemp readelf sha256sum sort stat tar timeout \
    update-desktop-database
do
    require_command "${command_name}"
done

[[ "$(uname -s)" == "Linux" ]] || fail "Debian packages must be validated on Linux"
[[ -f "${package_path}" ]] || fail "package not found: ${package_path}"
dpkg-deb --info "${package_path}" >/dev/null

metadata="$(cargo metadata \
    --locked \
    --manifest-path "${repo_root}/Cargo.toml" \
    --no-deps \
    --format-version 1)"
upstream_version="$(jq -er '
    [.packages[] | select(.name == "viewr") | .version]
    | if length == 1 then .[0] else error("expected exactly one viewr package") end
' <<<"${metadata}")"
expected_version="${upstream_version}-1"

[[ "$(dpkg-deb --field "${package_path}" Package)" == "viewr" ]] \
    || fail "unexpected Debian package name"
[[ "$(dpkg-deb --field "${package_path}" Version)" == "${expected_version}" ]] \
    || fail "package version does not match workspace version ${expected_version}"
[[ "$(dpkg-deb --field "${package_path}" Architecture)" == "amd64" ]] \
    || fail "unexpected Debian architecture"
dependencies="$(dpkg-deb --field "${package_path}" Depends)"
grep -Eq '(^|, )libvulkan1([[:space:](,]|$)' <<<"${dependencies}" \
    || fail "package does not depend on libvulkan1"
grep -Eq '(^|, )shared-mime-info([[:space:](,]|$)' <<<"${dependencies}" \
    || fail "package does not depend on shared-mime-info"
grep -Eq '(^|, )desktop-file-utils([[:space:](,]|$)' <<<"${dependencies}" \
    || fail "package does not depend on desktop-file-utils"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/viewr-deb-validation.XXXXXXXX")"
cleanup() {
    if [[ -n "${work_dir:-}" && -d "${work_dir}" ]]; then
        rm -rf -- "${work_dir}"
    fi
}
trap cleanup EXIT

package_root="${work_dir}/package"
control_root="${work_dir}/control"
mkdir -p -- "${package_root}" "${control_root}"
dpkg-deb --extract "${package_path}" "${package_root}"
dpkg-deb --control "${package_path}" "${control_root}"

actual_files="$(
    cd "${package_root}"
    find . -type f -printf '/%P\n' | LC_ALL=C sort
)"
expected_files="$(cat <<'EOF'
/usr/bin/viewr
/usr/share/applications/viewr-arw.desktop
/usr/share/applications/viewr.desktop
/usr/share/doc/viewr/LICENSE
/usr/share/doc/viewr/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html
/usr/share/doc/viewr/SOURCE-BUILD.md
/usr/share/doc/viewr/THIRD-PARTY-LICENSES.txt
/usr/share/doc/viewr/THIRD-PARTY-NOTICES.txt
/usr/share/doc/viewr/changelog.Debian.gz
/usr/share/doc/viewr/changelog.gz
/usr/share/doc/viewr/copyright
/usr/share/doc/viewr/rawler-LICENSE
/usr/share/doc/viewr/source-archive
EOF
)"
[[ "${actual_files}" == "${expected_files}" ]] || {
    printf 'Unexpected package payload.\nExpected:\n%s\nActual:\n%s\n' \
        "${expected_files}" "${actual_files}" >&2
    exit 1
}

actual_control_files="$(
    cd "${control_root}"
    find . -type f -printf '%P\n' | LC_ALL=C sort
)"
expected_control_files="$(printf 'control\nmd5sums')"
[[ "${actual_control_files}" == "${expected_control_files}" ]] \
    || fail "package contains unexpected control or maintainer scripts"
[[ ! -e "${package_root}/usr/share/applications/mimeapps.list" ]] \
    || fail "package must not install MIME defaults"

[[ "$(stat -c '%a' "${package_root}/usr/bin/viewr")" == "755" ]] \
    || fail "Viewr executable mode is not 0755"
for data_file in \
    "${package_root}/usr/share/applications/viewr-arw.desktop" \
    "${package_root}/usr/share/applications/viewr.desktop" \
    "${package_root}/usr/share/doc/viewr/LICENSE" \
    "${package_root}/usr/share/doc/viewr/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" \
    "${package_root}/usr/share/doc/viewr/SOURCE-BUILD.md" \
    "${package_root}/usr/share/doc/viewr/THIRD-PARTY-LICENSES.txt" \
    "${package_root}/usr/share/doc/viewr/THIRD-PARTY-NOTICES.txt" \
    "${package_root}/usr/share/doc/viewr/changelog.Debian.gz" \
    "${package_root}/usr/share/doc/viewr/changelog.gz" \
    "${package_root}/usr/share/doc/viewr/copyright" \
    "${package_root}/usr/share/doc/viewr/rawler-LICENSE" \
    "${package_root}/usr/share/doc/viewr/source-archive"
do
    [[ "$(stat -c '%a' "${data_file}")" == "644" ]] \
        || fail "data file mode is not 0644: ${data_file}"
done

if dpkg-deb --fsys-tarfile "${package_path}" \
    | tar -tvf - \
    | awk '$2 != "root/root" { found = 1 } END { exit found ? 0 : 1 }'
then
    fail "package payload contains a non-root owner or group"
fi

(
    cd "${package_root}"
    md5sum --check "${control_root}/md5sums"
)
gzip -t "${package_root}/usr/share/doc/viewr/changelog.Debian.gz"
gzip -t "${package_root}/usr/share/doc/viewr/changelog.gz"
cmp --silent "${repo_root}/LICENSE" \
    "${package_root}/usr/share/doc/viewr/LICENSE" \
    || fail "package LICENSE does not exactly match the repository LICENSE"
cmp --silent "${repo_root}/LICENSE" \
    "${package_root}/usr/share/doc/viewr/copyright" \
    || fail "package copyright does not exactly match the repository LICENSE"
cmp --silent "${repo_root}/packaging/THIRD-PARTY-NOTICES.txt" \
    "${package_root}/usr/share/doc/viewr/THIRD-PARTY-NOTICES.txt" \
    || fail "package third-party notice does not match the release notice"
cmp --silent "${repo_root}/packaging/THIRD-PARTY-LICENSES.txt" \
    "${package_root}/usr/share/doc/viewr/THIRD-PARTY-LICENSES.txt" \
    || fail "package third-party licenses do not match the generated inventory"
cmp --silent \
    "${repo_root}/packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" \
    "${package_root}/usr/share/doc/viewr/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html" \
    || fail "package Rust notices do not match the pinned toolchain copy"
cmp --silent "${repo_root}/packaging/SOURCE-BUILD.md" \
    "${package_root}/usr/share/doc/viewr/SOURCE-BUILD.md" \
    || fail "package source build instructions do not match the release instructions"
[[ "$(sha256sum "${package_root}/usr/share/doc/viewr/rawler-LICENSE" | awk '{print $1}')" == \
    "c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10" ]] \
    || fail "package does not contain the exact rawler 0.7.2 LICENSE"

launcher_file="${package_root}/usr/share/applications/viewr.desktop"
handler_file="${package_root}/usr/share/applications/viewr-arw.desktop"
desktop-file-validate "${launcher_file}"
desktop-file-validate "${handler_file}"
grep -Fxq 'Exec=viewr --pick-folder' "${launcher_file}" \
    || fail "desktop launcher must open Viewr's folder picker"
if grep -Fxq 'NoDisplay=true' "${launcher_file}"; then
    fail "desktop launcher must be visible in application menus"
fi
grep -Fxq 'Exec=viewr %f' "${handler_file}" \
    || fail "desktop handler must pass one selected file to Viewr"
grep -Fxq 'NoDisplay=true' "${handler_file}" \
    || fail "desktop handler must remain hidden from application menus"
grep -Fxq 'MimeType=image/x-sony-arw;' "${handler_file}" \
    || fail "desktop handler does not register the Sony ARW MIME type"
grep -Fq 'image/x-sony-arw:' /usr/share/mime/globs2 \
    || fail "host shared-mime-info does not define image/x-sony-arw"

# This updates only the temporary extracted tree. It never reads or writes the
# invoking user's mimeapps.list.
update-desktop-database "${package_root}/usr/share/applications"
grep -Fxq 'image/x-sony-arw=viewr-arw.desktop;' \
    "${package_root}/usr/share/applications/mimeinfo.cache" \
    || fail "desktop MIME cache does not expose Viewr as an ARW handler"

binary="${package_root}/usr/bin/viewr"
elf_header="$(readelf -h "${binary}")"
grep -Fq 'Class:                             ELF64' <<<"${elf_header}" \
    || fail "packaged executable is not ELF64"
grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' <<<"${elf_header}" \
    || fail "packaged executable is not x86-64"
if readelf --wide --sections "${binary}" | grep -Eq '\.(debug_|symtab)'; then
    fail "packaged executable contains debug or symbol table sections"
fi
if readelf -d "${binary}" | grep -Eq '\((RPATH|RUNPATH)\)'; then
    fail "packaged executable contains RPATH or RUNPATH"
fi
ldd_output="$(ldd "${binary}")"
if grep -Fq 'not found' <<<"${ldd_output}"; then
    printf '%s\n' "${ldd_output}" >&2
    fail "packaged executable has unresolved shared libraries"
fi

usage_output="$(timeout 10s "${binary}" 2>&1)" \
    || fail "packaged executable usage smoke test failed"
grep -Fq 'usage: viewr <folder|file.arw>' <<<"${usage_output}" \
    || fail "packaged executable did not print Viewr usage"

lintian --fail-on error "${package_path}"
printf 'Validated %s (%s, amd64)\n' "${package_path}" "${expected_version}"
