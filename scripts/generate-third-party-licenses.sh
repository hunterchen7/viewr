#!/usr/bin/env bash
set -euo pipefail

readonly CARGO_ABOUT_VERSION="0.9.1"
readonly RUST_TOOLCHAIN_CHANNEL="1.96"

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-licenses.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT

cd "$repository_root"

actual_about_version="$(cargo about --version)"
expected_about_version="cargo-about ${CARGO_ABOUT_VERSION}"
if [[ "$actual_about_version" != "$expected_about_version" ]]; then
    echo "error: expected ${expected_about_version}; found ${actual_about_version}" >&2
    exit 1
fi

if ! grep -Fqx "channel = \"${RUST_TOOLCHAIN_CHANNEL}\"" rust-toolchain.toml; then
    echo "error: rust-toolchain.toml must pin Rust ${RUST_TOOLCHAIN_CHANNEL}" >&2
    exit 1
fi

actual_rust_release="$(rustc --version --verbose | awk '$1 == "release:" { print $2 }')"
case "$actual_rust_release" in
    "$RUST_TOOLCHAIN_CHANNEL" | "$RUST_TOOLCHAIN_CHANNEL".*) ;;
    *)
        echo "error: expected Rust ${RUST_TOOLCHAIN_CHANNEL}.x; found ${actual_rust_release}" >&2
        exit 1
        ;;
esac

export LC_ALL=C
export SOURCE_DATE_EPOCH=0
export TZ=UTC

generated_licenses="${temporary_directory}/THIRD-PARTY-LICENSES.txt"
cargo about generate \
    --config about.toml \
    --manifest-path Cargo.toml \
    --workspace \
    --offline \
    --locked \
    --fail \
    --output-file "$generated_licenses" \
    about.hbs

# cargo-about preserves the line endings in upstream license files. Normalize
# them so macOS and Linux generate the same byte-for-byte inventory.
LC_ALL=C perl -pi -e 's/\r\n?/\n/g' "$generated_licenses"

# cargo-about excludes path-dependency crates, but the in-tree rawler fork
# (vendor/dnglab, bit-identical performance fusion of upstream 0.7.2) still
# ships under LGPL-2.1 and must stay in the inventory. Re-insert its overview
# entry (alphabetically before rayon) and its LGPL used-by attribution.
LC_ALL=C perl -0pi -e '
  s/^(rayon [0-9])/rawler 0.7.2\nLicense: LGPL-2.1\nSource: https:\/\/github.com\/hunterchen7\/dnglab\n$1/m
    or die "missing rayon overview anchor for the rawler license entry";
  s/(GNU Lesser General Public License v2\.1 only\n\nUsed by:\n)/$1- rawler 0.7.2\n/
    or die "missing LGPL used-by anchor for the rawler license entry";
' "$generated_licenses"

rust_sysroot="$(rustc --print sysroot)"
rust_copyright_source="${rust_sysroot}/share/doc/rust/COPYRIGHT-library.html"
if [[ ! -f "$rust_copyright_source" ]]; then
    echo "error: Rust standard-library copyright file is missing: ${rust_copyright_source}" >&2
    exit 1
fi

cp "$generated_licenses" packaging/THIRD-PARTY-LICENSES.txt
cp "$rust_copyright_source" packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html

echo "Generated packaging/THIRD-PARTY-LICENSES.txt"
echo "Copied packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
