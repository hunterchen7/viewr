#!/usr/bin/env bash
set -euo pipefail

readonly CARGO_ABOUT_VERSION="0.9.1"
readonly RUST_TOOLCHAIN_CHANNEL="1.96"
readonly JPEG_RUSTURBO_VERSION="0.9.2"
readonly JPEG_RUSTURBO_NOTICE_SHA256="fe5e4bf805fbfb2f4f5443decec492c801722a1b4376eb4878d7edf99cc697eb"
readonly JPEG_RUSTURBO_NOTICE_MARKER="----- BEGIN EXACT jpeg-rusturbo 0.9.2 NOTICE.md -----"

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/viewr-license-validation.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT

cd "$repository_root"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "error: sha256sum or shasum is required" >&2
        return 1
    fi
}

extract_jpeg_rusturbo_notice() {
    local notices_path="$1"
    local destination_path="$2"
    local marker_count
    local marker_position
    local marker_line
    local notice_start_line

    if [[ ! -f "$notices_path" ]]; then
        echo "error: third-party notice file is missing: ${notices_path}" >&2
        return 1
    fi
    if ! LC_ALL=C tr -d '\000' <"$notices_path" | cmp -s "$notices_path" -; then
        echo "error: third-party notice file contains NUL bytes: ${notices_path}" >&2
        return 1
    fi

    marker_count="$(
        LC_ALL=C grep -a -Fxc -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path" || true
    )"
    if [[ "$marker_count" != "1" ]]; then
        echo "error: ${notices_path} must contain exactly one jpeg-rusturbo NOTICE marker" >&2
        return 1
    fi

    marker_position="$(
        LC_ALL=C grep -a -Fnx -- "$JPEG_RUSTURBO_NOTICE_MARKER" "$notices_path"
    )"
    marker_line="${marker_position%%:*}"
    if [[ ! "$marker_line" =~ ^[1-9][0-9]*$ ]]; then
        echo "error: third-party notice marker has an invalid line number" >&2
        return 1
    fi
    notice_start_line=$((10#$marker_line + 1))
    tail -n "+$notice_start_line" "$notices_path" >"$destination_path"
}

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

metadata_path="${temporary_directory}/cargo-metadata.json"
cargo metadata \
    --manifest-path Cargo.toml \
    --locked \
    --offline \
    --format-version 1 >"$metadata_path"

jpeg_rusturbo_manifest="$(
    jq -er \
        --arg expected_version "$JPEG_RUSTURBO_VERSION" \
        --arg expected_manifest "${repository_root}/thirdparty/jpeg-rusturbo/Cargo.toml" \
        '
        [.packages[] | select(.name == "jpeg-rusturbo")] as $packages
        | if ($packages | length) != 1 then
            error("expected exactly one jpeg-rusturbo package")
          elif $packages[0].version != $expected_version then
            error(
              "expected jpeg-rusturbo "
              + $expected_version
              + ", found "
              + $packages[0].version
            )
          elif $packages[0].source != null
            or $packages[0].manifest_path != $expected_manifest
          then
            error("jpeg-rusturbo must resolve from the reviewed in-tree fork")
          else
            $packages[0].manifest_path
          end
        ' \
        "$metadata_path"
)"
jpeg_rusturbo_notice="$(dirname "$jpeg_rusturbo_manifest")/NOTICE.md"
if [[ ! -f "$jpeg_rusturbo_notice" ]]; then
    echo "error: jpeg-rusturbo ${JPEG_RUSTURBO_VERSION} NOTICE is missing" >&2
    exit 1
fi
actual_jpeg_rusturbo_notice_sha256="$(sha256_file "$jpeg_rusturbo_notice")"
if [[ "$actual_jpeg_rusturbo_notice_sha256" != "$JPEG_RUSTURBO_NOTICE_SHA256" ]]; then
    echo "error: jpeg-rusturbo ${JPEG_RUSTURBO_VERSION} NOTICE has an unexpected SHA-256" >&2
    exit 1
fi

packaged_jpeg_rusturbo_notice="${temporary_directory}/jpeg-rusturbo-NOTICE.md"
extract_jpeg_rusturbo_notice \
    "packaging/THIRD-PARTY-NOTICES.txt" \
    "$packaged_jpeg_rusturbo_notice"
if ! cmp -s "$jpeg_rusturbo_notice" "$packaged_jpeg_rusturbo_notice"; then
    echo "error: packaging/THIRD-PARTY-NOTICES.txt does not end with the exact jpeg-rusturbo ${JPEG_RUSTURBO_VERSION} NOTICE" >&2
    exit 1
fi

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

# Mirror generate-third-party-licenses.sh for both reviewed path dependencies.
LC_ALL=C perl -0pi -e '
  s/^(jxl-bitstream [0-9])/jpeg-rusturbo 0.9.2\nLicense: MIT OR Apache-2.0\nSource: https:\/\/github.com\/hunterchen7\/viewr\/tree\/main\/thirdparty\/jpeg-rusturbo (forked from https:\/\/github.com\/naoto256\/jpeg-rusturbo)\n$1/m
    or die "missing jxl-bitstream overview anchor for the jpeg-rusturbo license entry";
  s/(^- as-raw-xcb-connection [^\n]+\n)/$1- jpeg-rusturbo 0.9.2\n/m
    or die "missing Apache used-by anchor for the jpeg-rusturbo license entry";
  s/^(rayon [0-9])/rawler 0.7.2\nLicense: LGPL-2.1\nSource: https:\/\/github.com\/hunterchen7\/dnglab\n$1/m
    or die "missing rayon overview anchor for the rawler license entry";
  s/(GNU Lesser General Public License v2\.1 only\n\nUsed by:\n)/$1- rawler 0.7.2\n/
    or die "missing LGPL used-by anchor for the rawler license entry";
' "$generated_licenses"

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

echo "Third-party license files are current and reproducible."
