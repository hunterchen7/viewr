# Installers and releases

Viewr supplies one native installer for each release platform.

| Platform | Installer | Portable file |
|---|---|---|
| macOS 11 or later, Apple Silicon | `viewr-macos-arm64.pkg` | `viewr-macos-arm64.tar.gz` |
| Windows 10 or later, x64 | `viewr-windows-x64.msi` | `viewr-windows-x64.zip` |
| Ubuntu 22.04+ or Debian 12+, x64 | `viewr-linux-x64.deb` | `viewr-linux-x64.tar.gz` |

Each installer registers Viewr as an available Sony ARW viewer. No installer
changes the current default viewer.

## Set the default ARW viewer

### macOS

1. Select an ARW file in Finder.
2. Select **File > Get Info**.
3. Select **Viewr** in **Open with**.
4. Select **Change All**.

### Windows

1. Open **Settings**.
2. Select **Apps > Default apps**.
3. Search for `.arw`.
4. Select **Viewr**.

### Linux

Run these commands after installation:

```bash
xdg-mime default viewr-arw.desktop image/x-sony-arw
xdg-mime query default image/x-sony-arw
```

The second command must print `viewr-arw.desktop`.

## Build and validate an installer

The package scripts use the version in the workspace `Cargo.toml`. A release
build also compares this version with the GitHub release tag.

### macOS

Run these commands on an Apple Silicon Mac:

```bash
cargo build --release --locked --target aarch64-apple-darwin -p viewr --bin viewr
scripts/package-macos-pkg.sh \
  target/aarch64-apple-darwin/release/viewr \
  dist/viewr-macos-arm64.pkg
scripts/validate-macos-pkg.sh \
  --test-open-events \
  dist/viewr-macos-arm64.pkg
```

The validator checks the package layout, bundle data, ARW declaration,
deployment target, signatures, licenses, and two sequential open events.

Set `VIEWR_MACOS_APP_SIGN_IDENTITY` to use a Developer ID Application
identity. Set `VIEWR_MACOS_INSTALLER_SIGN_IDENTITY` to use a Developer ID
Installer identity. The script uses ad-hoc app signatures and an unsigned
package when these values are absent.

The script does not notarize or staple the package. Add those release steps
only after the repository has protected Apple signing credentials.

### Windows

Use Windows Server 2025 or Windows 11 with WiX 3.14.1:

```powershell
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release --locked `
  --target x86_64-pc-windows-msvc `
  -p viewr `
  --bin viewr

./scripts/build-windows-msi.ps1 `
  -BinaryPath target/x86_64-pc-windows-msvc/release/viewr.exe `
  -OutputDirectory dist

./scripts/test-windows-msi.ps1 `
  -MsiPath dist/viewr-windows-x64.msi `
  -ExpectedBinaryPath target/x86_64-pc-windows-msvc/release/viewr.exe `
  -AllowMachineChanges
```

Run the integration test only in an elevated, disposable environment. The
test refuses to run unless you set `-AllowMachineChanges`. It also refuses to
continue when Viewr is already installed.

The test installs and removes the MSI. It checks files, shortcuts, package
data, registry data, command quoting, association notifications, executable
identity, static Microsoft C runtime linkage, and preservation of an unrelated
ARW default.

The MSI and executable are unsigned until protected Authenticode credentials
are available. Sign and timestamp both files before a trusted production
release.

### Debian or Ubuntu

Install the package tools and Viewr build dependencies:

```bash
sudo apt-get update
sudo apt-get install -y \
  binutils desktop-file-utils dpkg-dev jq libgtk-3-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxkbcommon-dev libvulkan1 lintian shared-mime-info xdg-utils
```

Build and validate the package:

```bash
cargo build --release --locked \
  --target x86_64-unknown-linux-gnu \
  -p viewr \
  --bin viewr

scripts/package-linux-deb.sh \
  --binary target/x86_64-unknown-linux-gnu/release/viewr \
  --output-dir dist

scripts/validate-linux-deb.sh dist/viewr-linux-x64.deb
scripts/test-linux-deb-install.sh \
  --allow-system-changes \
  dist/viewr-linux-x64.deb
```

The structural validator checks package data, files, permissions, MIME data,
ELF data, linked libraries, and lintian errors. The integration test installs
and purges the package on a disposable CI host. It checks desktop-handler
registration and verifies that the current user's MIME default does not
change.

## Build the release source

Run this procedure on Linux:

```bash
scripts/package-source-archive.sh dist
scripts/validate-source-archive.sh dist/viewr-*-source.tar.gz
```

The source archive contains the repository files and versioned, vendored Rust
dependencies. The validator checks offline Cargo metadata, the exact rawler
license, and an offline compile after it prepares and edits a local rawler
replacement. See `packaging/SOURCE-BUILD.md` inside the archive.

## License inventories

Each native installer and portable archive contains:

- The Viewr MIT license.
- `THIRD-PARTY-NOTICES.txt`.
- The generated `THIRD-PARTY-LICENSES.txt` dependency inventory.
- The Rust 1.96 standard-library copyright inventory.
- The exact rawler 0.7.2 LGPL license.
- Offline source-build instructions.

CI uses cargo-about 0.9.1 with a locked union of all three release targets. An
unknown or unreviewed license fails CI. CI also verifies that the checked-in
Rust standard-library inventory matches the pinned toolchain.

## Release publication

Release-please creates a draft release and its tag, then calls the release
workflow directly. A repository-token release event is not used because those
events do not start another GitHub Actions workflow. The release workflow can
also be run manually for an existing draft tag.

Three isolated jobs build and validate the platform files. A fourth job builds
and validates the source archive.

The publication job starts only after all four jobs pass. It checks the exact
local and remote file sets and every remote SHA-256 digest, creates
`SHA256SUMS`, creates GitHub provenance attestations for every artifact and for
the checksum manifest, and uploads all files in one release command. A
per-tag concurrency lock prevents two publication attempts from interleaving.
The workflow publishes the draft only after the final remote verification
passes. Every external GitHub Action is pinned to a reviewed commit.

To retry a failed draft release, dispatch the workflow at the release tag:

```bash
release_tag=v0.2.0
gh workflow run release-binaries.yml \
  --ref "$release_tag" \
  -f release_tag="$release_tag"
```

The workflow rejects a release tag whose commit does not match the workflow
invocation commit. This keeps artifact provenance tied to the released source.

Verify an attestation with:

```bash
gh attestation verify PATH-TO-DOWNLOAD -R hunterchen7/viewr
```

Verify `SHA256SUMS` with the same command before you use it as a checksum
manifest. GitHub provenance does not replace Apple or Microsoft platform
signing. Until protected signing credentials are configured, the `.pkg` and
`.msi` are preview installers and the operating system can warn about them.
