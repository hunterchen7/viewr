#!/usr/bin/env bash
#
# Build a reproducible-friendly amd64 Debian package from an existing Viewr
# release binary. This script intentionally does not install the package or
# change any user's MIME preferences.

set -euo pipefail
export LC_ALL=C

usage() {
    cat <<'EOF'
Usage: scripts/package-linux-deb.sh [options]

Options:
  --binary PATH       Viewr ELF binary (default: target/release/viewr)
  --output-dir PATH   Package output directory (default: dist)
  --release-tag TAG   Assert that TAG is v<workspace-version>
  -h, --help          Show this help

Environment:
  SOURCE_DATE_EPOCH   Archive timestamp. Defaults to the HEAD commit time.
  RAWLER_LICENSE_PATH Override the packaged rawler 0.7.2 LICENSE path.
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/.." && pwd -P)"
binary_path="${repo_root}/target/release/viewr"
output_dir="${repo_root}/dist"
release_tag=""

while (($# > 0)); do
    case "$1" in
        --binary)
            (($# >= 2)) || fail "--binary requires a path"
            binary_path="$2"
            shift 2
            ;;
        --output-dir)
            (($# >= 2)) || fail "--output-dir requires a path"
            output_dir="$2"
            shift 2
            ;;
        --release-tag)
            (($# >= 2)) || fail "--release-tag requires a tag"
            release_tag="$2"
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

for command_name in \
    awk cargo cat date dpkg dpkg-deb dpkg-shlibdeps du find git grep gzip \
    install jq md5sum mktemp readelf sed sha256sum sort touch tr uname xargs
do
    require_command "${command_name}"
done

[[ "$(uname -s)" == "Linux" ]] || fail "Debian packages must be built on Linux"
[[ "$(dpkg --print-architecture)" == "amd64" ]] \
    || fail "the Debian package currently supports only an amd64 build host"
[[ -f "${binary_path}" && -x "${binary_path}" ]] \
    || fail "Viewr binary is not an executable file: ${binary_path}"

elf_header="$(readelf -h "${binary_path}")"
grep -Fq 'Class:                             ELF64' <<<"${elf_header}" \
    || fail "Viewr binary is not ELF64"
grep -Fq 'Data:                              2'\''s complement, little endian' <<<"${elf_header}" \
    || fail "Viewr binary is not little-endian"
grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' <<<"${elf_header}" \
    || fail "Viewr binary is not x86-64"

metadata="$(cargo metadata \
    --locked \
    --manifest-path "${repo_root}/Cargo.toml" \
    --no-deps \
    --format-version 1)"
upstream_version="$(jq -er '
    [.packages[] | select(.name == "viewr") | .version]
    | if length == 1 then .[0] else error("expected exactly one viewr package") end
' <<<"${metadata}")"
debian_version="${upstream_version}-1"

rawler_license_path="${RAWLER_LICENSE_PATH:-${repo_root}/packaging/licenses/rawler-0.7.2-LICENSE}"
[[ -f "${rawler_license_path}" ]] \
    || fail "rawler 0.7.2 LICENSE not found: ${rawler_license_path}"
rawler_license_sha256="$(sha256sum "${rawler_license_path}" | awk '{print $1}')"
[[ "${rawler_license_sha256}" == \
    "c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10" ]] \
    || fail "located license is not the exact rawler 0.7.2 LICENSE"

if [[ -n "${release_tag}" && "${release_tag}" != "v${upstream_version}" ]]; then
    fail "release tag ${release_tag} does not match workspace version v${upstream_version}"
fi
dpkg --validate-version "${debian_version}" \
    || fail "invalid Debian version: ${debian_version}"

source_date_epoch="${SOURCE_DATE_EPOCH:-}"
if [[ -z "${source_date_epoch}" ]]; then
    source_date_epoch="$(git -C "${repo_root}" log -1 --format=%ct)"
fi
[[ "${source_date_epoch}" =~ ^[0-9]+$ ]] \
    || fail "SOURCE_DATE_EPOCH must be a non-negative integer"
export SOURCE_DATE_EPOCH="${source_date_epoch}"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/viewr-deb.XXXXXXXX")"
cleanup() {
    if [[ -n "${work_dir:-}" && -d "${work_dir}" ]]; then
        rm -rf -- "${work_dir}"
    fi
}
trap cleanup EXIT

package_root="${work_dir}/package"
install -d -m 0755 \
    "${package_root}/DEBIAN" \
    "${package_root}/usr/bin" \
    "${package_root}/usr/share/applications" \
    "${package_root}/usr/share/doc/viewr"
install -m 0755 "${binary_path}" "${package_root}/usr/bin/viewr"
install -m 0644 \
    "${repo_root}/packaging/linux/viewr.desktop" \
    "${package_root}/usr/share/applications/viewr.desktop"
install -m 0644 "${repo_root}/LICENSE" \
    "${package_root}/usr/share/doc/viewr/LICENSE"
install -m 0644 "${repo_root}/LICENSE" \
    "${package_root}/usr/share/doc/viewr/copyright"
install -m 0644 "${rawler_license_path}" \
    "${package_root}/usr/share/doc/viewr/rawler-LICENSE"
install -m 0644 "${repo_root}/packaging/THIRD-PARTY-NOTICES.txt" \
    "${package_root}/usr/share/doc/viewr/THIRD-PARTY-NOTICES.txt"
install -m 0644 "${repo_root}/packaging/SOURCE-BUILD.md" \
    "${package_root}/usr/share/doc/viewr/SOURCE-BUILD.md"

cargo_lock_sha256="$(sha256sum "${repo_root}/Cargo.lock" | awk '{print $1}')"
cat >"${package_root}/usr/share/doc/viewr/source-archive" <<EOF
Viewr ${upstream_version} corresponding source
===============================================

Download the release's vendored source archive here:
  https://github.com/hunterchen7/viewr/releases/download/v${upstream_version}/viewr-${upstream_version}-source.tar.gz

Cargo.lock SHA-256 for this build:
  ${cargo_lock_sha256}

See SOURCE-BUILD.md in this directory for offline rebuild instructions.
EOF

gzip -n -9 -c "${repo_root}/CHANGELOG.md" \
    >"${package_root}/usr/share/doc/viewr/changelog.gz"
debian_date="$(LC_ALL=C date -u -d "@${source_date_epoch}" --rfc-email)"
cat >"${work_dir}/changelog.Debian" <<EOF
viewr (${debian_version}) unstable; urgency=medium

  * Package Viewr ${upstream_version} for amd64 Linux.

 -- Viewr maintainers <hunterchen7@users.noreply.github.com>  ${debian_date}
EOF
gzip -n -9 -c "${work_dir}/changelog.Debian" \
    >"${package_root}/usr/share/doc/viewr/changelog.Debian.gz"

# dpkg-shlibdeps expects Debian package context. This temporary control file is
# only used to calculate dependencies from the exact binary being packaged.
install -d -m 0755 "${work_dir}/debian"
cat >"${work_dir}/debian/control" <<'EOF'
Source: viewr
Section: graphics
Priority: optional
Maintainer: Viewr maintainers <hunterchen7@users.noreply.github.com>

Package: viewr
Architecture: amd64
Description: Low-latency Sony ARW culling viewer
EOF
shlibs_output="$(
    cd "${work_dir}"
    dpkg-shlibdeps -O -e"${package_root}/usr/bin/viewr"
)"
shlibs_depends="${shlibs_output#shlibs:Depends=}"
[[ -n "${shlibs_depends}" && "${shlibs_depends}" != "${shlibs_output}" ]] \
    || fail "dpkg-shlibdeps did not produce dependency metadata"
depends="$(
    printf '%s\n' "${shlibs_depends}, libvulkan1, shared-mime-info" \
        | tr ',' '\n' \
        | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
        | sed '/^$/d' \
        | LC_ALL=C sort -u \
        | awk 'BEGIN { separator = "" } { printf "%s%s", separator, $0; separator = ", " } END { print "" }'
)"

installed_size="$(du -sk "${package_root}/usr" | awk '{print $1}')"
cat >"${package_root}/DEBIAN/control" <<EOF
Package: viewr
Version: ${debian_version}
Section: graphics
Priority: optional
Architecture: amd64
Maintainer: Viewr maintainers <hunterchen7@users.noreply.github.com>
Installed-Size: ${installed_size}
Depends: ${depends}
Homepage: https://github.com/hunterchen7/viewr
Description: Low-latency Sony ARW culling viewer
 Viewr is a desktop application for quickly viewing, comparing, and rating
 Sony ARW raw photographs.
EOF

(
    cd "${package_root}"
    find usr -type f -print0 \
        | LC_ALL=C sort -z \
        | xargs -0 md5sum >DEBIAN/md5sums
)

# Normalize every member timestamp before dpkg-deb assembles the ar archive.
find "${package_root}" -print0 \
    | xargs -0 touch --no-dereference --date="@${source_date_epoch}"

mkdir -p -- "${output_dir}"
package_name="viewr-linux-x64.deb"
temporary_package="${work_dir}/${package_name}"
dpkg-deb \
    --root-owner-group \
    -Zxz \
    -z9 \
    --build \
    "${package_root}" \
    "${temporary_package}"
install -m 0644 "${temporary_package}" "${output_dir}/${package_name}"

printf 'Built %s\n' "${output_dir}/${package_name}"
sha256sum "${output_dir}/${package_name}"
