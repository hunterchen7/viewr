#!/usr/bin/env bash
set -euo pipefail

# Historical helper: it used to copy the vendored crates.io rawler into
# local/ and add a [patch.crates-io] override so release-archive users could
# edit rawler. The rawler fork now lives in-tree as the vendor/dnglab
# submodule (staged with full contents in release source archives), and
# Cargo.toml already points the rawler patch at vendor/dnglab/rawler, so the
# source is directly editable in both the repository and extracted archives.

echo "rawler is already editable in-tree at vendor/dnglab/rawler" >&2
echo "(the Cargo.toml [patch.crates-io] override points there; edit and rebuild)" >&2
exit 1
