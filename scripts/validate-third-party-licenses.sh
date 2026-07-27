#!/usr/bin/env bash
set -euo pipefail

readonly CARGO_ABOUT_VERSION="0.9.1"
readonly RUST_TOOLCHAIN_CHANNEL="1.96"

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-license-validation.XXXXXX")"
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

# Match the platform-independent output produced by the generation script.
LC_ALL=C perl -pi -e 's/\r\n?/\n/g' "$generated_licenses"

if ! cmp -s "$generated_licenses" packaging/THIRD-PARTY-LICENSES.txt; then
    echo "error: packaging/THIRD-PARTY-LICENSES.txt is stale" >&2
    diff -u packaging/THIRD-PARTY-LICENSES.txt "$generated_licenses" || true
    exit 1
fi

rust_sysroot="$(rustc --print sysroot)"
rust_copyright_source="${rust_sysroot}/share/doc/rust/COPYRIGHT-library.html"
rust_copyright_copy="packaging/RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"

if [[ ! -f "$rust_copyright_source" ]]; then
    echo "error: Rust standard-library copyright file is missing: ${rust_copyright_source}" >&2
    exit 1
fi

if ! cmp -s "$rust_copyright_source" "$rust_copyright_copy"; then
    echo "error: ${rust_copyright_copy} does not match the pinned Rust toolchain" >&2
    exit 1
fi

if ! grep -Fqx \
    "This software is based in part on the work of the Independent JPEG Group." \
    packaging/THIRD-PARTY-NOTICES.txt ||
    ! grep -Fqx \
    "The TurboJPEG API and build system are available under the Modified (3-clause)" \
    packaging/THIRD-PARTY-NOTICES.txt ||
    ! grep -Fqx \
    "1. Redistributions of source code must retain the above copyright notice," \
    packaging/THIRD-PARTY-NOTICES.txt ||
    ! grep -Fqx \
    "2. Redistributions in binary form must reproduce the above copyright notice," \
    packaging/THIRD-PARTY-NOTICES.txt ||
    ! grep -Fqx \
    "3. Neither the name of the libjpeg-turbo Project nor the names of its" \
    packaging/THIRD-PARTY-NOTICES.txt ||
    ! grep -Fqx \
    'THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS",' \
    packaging/THIRD-PARTY-NOTICES.txt ||
    ! grep -Fqx \
    "Bundled by: turbojpeg-sys 1.2.0" \
    packaging/THIRD-PARTY-NOTICES.txt; then
    echo "error: libjpeg-turbo attribution is incomplete" >&2
    exit 1
fi

echo "Third-party license files are current and reproducible."
