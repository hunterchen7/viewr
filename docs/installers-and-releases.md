# Installers and releases

Viewr supplies one native installer for each release platform.

| Platform | Installer | Portable file |
|---|---|---|
| macOS 11 or later, Apple Silicon | `viewr-macos-arm64.pkg` | `viewr-macos-arm64.tar.gz` |
| Windows 10 or later, x64 | `viewr-windows-x64.msi` | `viewr-windows-x64.zip` |
| Ubuntu 22.04+ or Debian 12+, x64 | `viewr-linux-x64.deb` | `viewr-linux-x64.tar.gz` |

## In-app updates

Viewr checks the latest stable GitHub release. The Cargo package version is the
installed version. The release tag supplies the available version.

Viewr accepts only a canonical `vMAJOR.MINOR.PATCH` tag. Viewr does not accept a
draft release, a prerelease, or a build suffix. Viewr uses a semantic-version
comparison, not a text comparison.

An automatic check starts four seconds after the viewer opens. Viewr limits
automatic checks to one check each day. A file lock applies this limit to all
open Viewr processes. Use **Preferences > Updates > Check now** for a manual
check. The automatic-check preference is also shared by all open processes.
When one process disables the checks, another process checks the preference
again before it makes a network request.

The update dialog shows the bounded release text as plain text. The dialog has
these actions:

- **Download now** downloads the applicable package.
- **Later** closes the dialog for the current process.
- **Skip this version** stores the exact available version.
- **View release** opens the fixed GitHub release page.

A skipped version stays hidden during automatic checks. A later version appears
normally. A manual check can show a skipped version again.

### Package selection

Viewr selects an installer only when the current executable has a native
installer path:

- macOS: `/Applications/Viewr.app/Contents/MacOS/viewr-bin`.
- Windows: `Program Files\Viewr\viewr.exe`.
- Linux: `/usr/bin/viewr`.

For another executable path, Viewr selects the portable archive. Viewr downloads
the archive but does not replace the current executable.

The package names are a fixed application contract. The updater does not use a
file name from an HTTP header. The updater constructs the download URL from the
validated version and one of these fixed names.

### Download validation

The release workflow checks each remote asset before it publishes the release.
The workflow checks the asset state, byte size, and GitHub SHA-256 digest.

The updater applies the same checks. It also applies these controls:

- It permits HTTPS requests to known GitHub hosts only.
- It limits redirects, response sizes, asset counts, and request time.
- It writes the package to a same-directory temporary file.
- It calculates SHA-256 while it writes the package.
- It publishes the file only after the size and digest match.
- It adds macOS quarantine data or Windows Internet-zone data.
- It calculates SHA-256 and checks the download metadata again on a background
  thread before it opens an installer.

Viewr stores update state in the Viewr configuration directory. Viewr stores
downloads and cross-process lock files in the Viewr cache directory. The updater
rejects a symbolic-link state file, lock file, directory, or package target. On
Unix systems, the updater uses owner-only permissions for its state, lock, and
download paths.

Only one Viewr process can download an update at one time. When that process
starts a download, it removes temporary files that an interrupted process left
behind. It also removes completed version directories that are more than seven
days old. It keeps the directory for the requested version.

CAUTION: The preview installers do not have Apple or Microsoft platform
signatures. An untrusted installer can change the system. Confirm the repository
and the operating-system prompt before you continue.

The GitHub digest detects a changed or incomplete download. The digest and the
package use the same GitHub release trust boundary. Thus, the digest is not a
publisher signature. The updater always requires a user action before it opens
the operating-system installer.

### Installer handoff

Viewr opens the package with the applicable operating-system command:

- macOS: `/usr/bin/open PACKAGE.pkg`.
- Windows: the `msiexec.exe` in the Windows system directory.
- Linux: `/usr/bin/xdg-open PACKAGE.deb`.

The operating system controls permission prompts and installation. Viewr cannot
reliably detect completion for all three package types. Thus, Viewr does not
claim that it can install and restart automatically.

After the installer opens, close all Viewr windows. Complete the installation.
Then open Viewr again. A normal Viewr close lets rating and cache workers finish
their shutdown work.

Each installer registers Viewr as an available Sony ARW viewer. No installer
overwrites an explicit user choice for the default viewer.

On a macOS account without an explicit ARW choice, Launch Services can select
Viewr as the inferred default after installation. The installer does not write
this choice to the user preferences.

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

Run these commands on an Apple Silicon Mac with macOS 12 or later:

```bash
cargo build --release --locked --target aarch64-apple-darwin -p viewr --bin viewr
scripts/package-macos-pkg.sh \
  target/aarch64-apple-darwin/release/viewr \
  dist/viewr-macos-arm64.pkg
scripts/validate-macos-pkg.sh \
  --test-open-events \
  dist/viewr-macos-arm64.pkg
VIEWR_TEST_RAW=/absolute/path/photo.ARW \
scripts/test-macos-pkg-install.sh \
  --allow-system-changes \
  dist/viewr-macos-arm64.pkg
```

The validator checks the package layout, arm64 support, and the macOS 11
requirement. It also checks bundle data, the ARW declaration, the deployment
target, signatures, and licenses. The open-event test checks one same-folder
batch and two later open events. Run the install test only on a disposable Mac.
The test installs the package in `/Applications`. It checks the receipt,
payload, permissions, command, Launch Services registration, and handler
preferences. It uses the ARW file to test Finder-equivalent default routing.
It verifies that the file contents do not change. It then removes the app and
receipt.

Use a valid Sony ARW file for `VIEWR_TEST_RAW`. CI downloads the pinned
public-domain fixture that the core compatibility tests use.

The install test requires exact preservation of explicit Launch Services
preferences. On an account without an explicit ARW choice, Launch Services can
recompute its inferred default while Viewr is installed. The test verifies
that removal restores the prior effective default.

The installer supports macOS 11. The install test requires macOS 12 because it
uses newer Launch Services inspection APIs.

Set `VIEWR_MACOS_APP_SIGN_IDENTITY` to use a Developer ID Application
identity. Set `VIEWR_MACOS_INSTALLER_SIGN_IDENTITY` to use a Developer ID
Installer identity. The script uses ad-hoc app signatures and an unsigned
package when these values are absent.

The script does not notarize or staple the package. Add those release steps
only after the repository has protected Apple signing credentials.

To remove the macOS package manually, run:

```bash
launch_services=\
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
"$launch_services" -u /Applications/Viewr.app
sudo /bin/rm -rf -- /Applications/Viewr.app
sudo /usr/sbin/pkgutil --forget com.hunterchen.viewr.pkg
```

These commands remove only the installed app, package receipt, and Launch
Services registration. They do not remove user settings, caches, or ratings.

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
stripped ELF data, linked libraries, and lintian errors. The integration test
installs and purges the package on a disposable CI host. It checks
desktop-handler registration and verifies that the current user's MIME
default does not change.

## Build and validate a portable file

On macOS, run:

```bash
scripts/package-portable-archive.sh \
  --platform macos-arm64 \
  --binary target/aarch64-apple-darwin/release/viewr \
  --output dist/viewr-macos-arm64.tar.gz
scripts/validate-portable-archive.sh \
  --platform macos-arm64 \
  --archive dist/viewr-macos-arm64.tar.gz \
  --expected-binary target/aarch64-apple-darwin/release/viewr
```

On Linux, use `linux-x64`, `viewr-linux-x64.tar.gz`, and the
`x86_64-unknown-linux-gnu` binary path with the same two scripts.

On Windows, run:

```powershell
./scripts/package-portable-archive.ps1 `
  -Platform windows-x64 `
  -BinaryPath target/x86_64-pc-windows-msvc/release/viewr.exe `
  -OutputPath dist/viewr-windows-x64.zip
./scripts/validate-portable-archive.ps1 `
  -Platform windows-x64 `
  -ArchivePath dist/viewr-windows-x64.zip `
  -ExpectedBinaryPath target/x86_64-pc-windows-msvc/release/viewr.exe
```

The Windows validator requires 7-Zip on `PATH`. CI builds and validates the
archive under Windows PowerShell 5.1 and PowerShell 7. Repeated builds must be
byte-identical within each runtime; both runtimes must produce the same exact
validated file set and contents.

The validators require the exact binary and license file set. They reject
extra files, changed file contents, and an incorrect binary architecture. The
tar validator also checks file modes.

## Build the release source

Run this procedure on Linux:

```bash
scripts/package-source-archive.sh dist
scripts/validate-source-archive.sh dist/viewr-*-source.tar.gz
```

The source archive contains the repository files and versioned, vendored Rust
dependencies. CI requires two source-package builds to be byte-identical. The
validator rejects duplicate, noncanonical, linked, and special members. It
checks offline Cargo metadata for both the app and JPEG benchmark workspaces,
the source-only codec license files, and the benchmark tests. The validator also
checks the exact rawler license and the exact `jpeg-rusturbo` notice. It then
prepares and edits a local rawler replacement before an offline release link.
The generated third-party inventory covers the shipped app dependency graph.
Experimental benchmark crates retain licenses with their vendored source. See
`packaging/SOURCE-BUILD.md` in the archive.

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

Some dependencies contain required notices that Cargo license metadata does not
describe. CI locates `jpeg-rusturbo` 0.9.2 in locked Cargo metadata. It pins the
upstream `NOTICE.md` by SHA-256 and requires its exact bytes at the end of
`THIRD-PARTY-NOTICES.txt`. The source validator applies the same check to the
vendored notice.

## Release publication

Release-please creates a draft release and its tag, then calls the release
workflow directly. A repository-token release event is not used because those
events do not start another GitHub Actions workflow. The release workflow can
also be run manually for an existing draft tag.

The release gate gives GitHub's authenticated release list up to three minutes
to expose the new stable draft. It records the draft's numeric release ID and
uses that identity for later state and asset checks. It then waits for the
complete five-check main-branch CI workflow to pass for the exact tagged
commit. It does not rerun those checks. Three isolated jobs then build and
validate the platform files. A fourth job builds and validates the source
archive. New main-branch pushes do not cancel an earlier commit's
release-eligible CI run.

GitHub hides draft releases from read-only repository tokens. The gate
therefore needs `contents: write` even though it does not mutate the release.
It checks out the workflow's immutable `main` commit as the working tree,
checks out the tagged source in a separate subdirectory, and does not persist
Git credentials in either checkout. Privileged release helpers therefore come
from the current workflow commit, including during recovery of an older tag.

The publication job starts only after all four jobs pass. It checks the exact
local and remote file sets and every remote SHA-256 digest, creates
`SHA256SUMS`, creates GitHub provenance attestations for every artifact and for
the checksum manifest, and uploads all files by the draft's numeric release ID.
A per-tag concurrency lock prevents two publication attempts from interleaving.
For a normal release, the workflow publishes that exact draft only after the
final remote verification passes. Publication verifies both the numeric
release identity and the immutable tag commit, and it retries bounded
post-publication reads. Every external GitHub Action is pinned to a reviewed
commit.

The in-app updater uses the asset state, size, and digest from the GitHub release
API. Thus, the publication job must continue to reject a missing digest. A
change to a platform package name also requires a matching application change.

To retry a failed draft release, dispatch the current `main` workflow for the
release tag:

```bash
release_tag=v0.2.0
gh workflow run release-binaries.yml \
  --ref main \
  -f release_tag="$release_tag"
```

The workflow itself must run from `main`. An automatic release requires the
release tag and workflow invocation to identify the same commit. A manual
dispatch can use the fixed workflow from `main` to recover an older draft, but
it still checks out the immutable tag and requires successful main-branch CI
for that exact tagged commit. All downstream jobs use that approved commit SHA.
Before the workflow uploads files, it verifies that the tag still identifies
the approved commit.

GitHub's workflow token cannot publish some historical releases when their
target commit changes workflow files. A historical recovery therefore uploads
and verifies the exact assets but deliberately leaves the release as a draft.
It creates a custom recovery attestation that records both the old source
commit and the current workflow commit. After the recovery run succeeds, a
maintainer downloads the immutable validated-artifact snapshot from that exact
run. The publication helper compares every current draft asset's name, size,
and GitHub-computed digest with that snapshot immediately before publication.
It also rechecks the tag and draft identity.

Run the exact command emitted in the successful recovery run summary. Its
expanded form is:

```bash
recovery_run_id=RECOVERY-RUN-ID
recovery_run_attempt=RECOVERY-RUN-ATTEMPT
workflow_sha=RECOVERY-WORKFLOW-COMMIT
release_id=361510019
release_sha=106284ee7dec2d9e05aa091121747adfa0642407
release_tag=v0.2.0
recovery_directory="$(mktemp -d)"

gh run download "$recovery_run_id" \
  --repo hunterchen7/viewr \
  --name "historical-release-$release_id-attempt-$recovery_run_attempt" \
  --dir "$recovery_directory/assets"
git clone --no-checkout \
  https://github.com/hunterchen7/viewr.git \
  "$recovery_directory/release-tools"
git -C "$recovery_directory/release-tools" \
  checkout --detach "$workflow_sha"

GH_TOKEN="$(gh auth token)" \
GITHUB_REPOSITORY=hunterchen7/viewr \
"$recovery_directory/release-tools/scripts/publish-draft-release.sh" \
  --asset-directory "$recovery_directory/assets" \
  --release-id "$release_id" \
  --release-sha "$release_sha" \
  --recovery-workflow-sha "$workflow_sha" \
  "$release_tag"
```

Only the final helper process receives the maintainer's `gh` token, which must
have `repo` and `workflow` scopes for this GitHub API case. The helper runs from
the recovery workflow's immutable commit, resolves annotated tags, rejects a
moved tag or replaced release, revalidates the exact assets, and publishes only
the numeric draft ID. It uses GitHub's legacy latest-release selection so an
older recovery cannot supersede a newer release. It validates the PATCH
response, uses a three-minute bounded final-state check, rechecks the tag after
publication, and restores the draft if that final tag check fails.

Verify a normal release attestation with the exact reusable-workflow signer:

```bash
gh attestation verify PATH-TO-DOWNLOAD \
  --repo hunterchen7/viewr \
  --signer-workflow hunterchen7/viewr/.github/workflows/release-binaries.yml
```

Historical recovery artifacts use a custom predicate. Verify its signature and
assert the recovery identity fields before publication:

```bash
predicate_type=https://github.com/hunterchen7/viewr/attestations/release-recovery/v1
verification="$(
  gh attestation verify PATH-TO-DOWNLOAD \
    --repo hunterchen7/viewr \
    --signer-workflow hunterchen7/viewr/.github/workflows/release-binaries.yml \
    --signer-digest "$workflow_sha" \
    --predicate-type "$predicate_type" \
    --format json
)"
jq -e \
  --argjson release_id "$release_id" \
  --arg release_sha "$release_sha" \
  --arg release_tag "$release_tag" \
  --arg workflow_sha "$workflow_sha" \
  'any(.[];
    .verificationResult.statement.predicate.release.id == $release_id
    and .verificationResult.statement.predicate.release.tag == $release_tag
    and .verificationResult.statement.predicate.release.sourceCommit == $release_sha
    and .verificationResult.statement.predicate.workflow.commit == $workflow_sha
  )' <<<"$verification"
```

Verify `SHA256SUMS` with the same command before you use it as a checksum
manifest. GitHub provenance does not replace Apple or Microsoft platform
signing. Until protected signing credentials are configured, the `.pkg` and
`.msi` are preview installers and the operating system can warn about them.
