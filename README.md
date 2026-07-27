# viewr

A fast RAW culling viewer. Built for burst photoshoots: flip through
hundreds of Sony ARWs with near-zero latency, progressively replace fast
embedded-preview placeholders with *actual decoded RAW* for focus judgement,
rate keepers 1–5 stars, then import into Lightroom and filter by rating — the
ratings live in Lightroom-compatible `.xmp` sidecars.

## Speed model

Work is demand-driven. The visible thumbnail viewport has a replaceable
priority lane, while main-image development follows a bounded outward wave
from the current image with a ~3:1 forward bias. When disk caching is enabled,
idle workers make a best-effort persistent pass to warm Browse renders across
the folder. Metadata is scanned separately, without decoding every embedded
preview.

Residency uses hard accounting limits and soft targets:

1. **GPU textures** — Browse textures cover the current ±2 visible images.
   While zoomed, Viewr overlays Full-resolution 1024-pixel tiles on the current
   image, uploading the visible region first and then expanding outward. The
   Browse texture remains underneath until each tile is ready. The viewport
   thumbnail LRU holds at most 256 MiB of logical RGBA bytes. The backend
   allocation can differ. Each frame uploads a maximum of eight new thumbnails.
2. **Decoded RGBA in RAM** — exact LRU with a target byte budget for instant
   display. Viewr preloads and pins Full renders for the current image and its
   immediate visible neighbors. Pinned images can keep a ring above its target
   until Viewr removes the pins.
3. **JPEG bytes in RAM** — memoized develops with a target byte budget, ~20×
   smaller and cheap to re-inflate. Pins have the same soft-budget behavior.
4. **Disk** (`~/Library/Caches/viewr/`) — a target budget for developed JPEGs
   used on fast folder reopens; cache GC enforces the target rather than every
   write, and files are never written inside photo folders.

Both develop tiers use real RAW data. Browse uses a half-resolution superpixel
demosaic. Full uses a full-resolution PPG demosaic. Fit mode schedules and pins
Full renders for the current image and its immediate visible neighbors. Viewr
does not upload Full pixels to the GPU until zoom needs them. Visible-region
tiling starts after the Full CPU render completes; the underlying PPG RAW
demosaic remains a whole-render operation. The main view can show an embedded
thumbnail while the Browse render is not ready.

The display pipeline applies the camera white balance and color matrix. It then
applies a small exposure lift, highlight roll-off, sRGB transfer, and tone
curve. This pipeline gives a useful culling image without changing the RAW file.

## Usage

```
viewr <folder|file.arw>        browse a folder of raws
viewr --pick-folder            choose a folder in a native dialog
viewr dev <file.arw> [out]     decode one file, print per-stage timings
```

| Key | Action |
|---|---|
| ← / → (Shift: ±10) | previous / next |
| Home / End | first / last |
| 0–5 | rate (0 clears) — writes `.xmp` sidecar |
| Space / Z / double-click | toggle fit ↔ 100% at cursor |
| pinch or Ctrl/Cmd+scroll | zoom at cursor |
| scroll or drag | pan (zoomed) |
| G (Enter in grid) | grid ↔ loupe |
| I | metadata panel |
| F | fullscreen |
| Cmd+O | open folder |
| Cmd+, | preferences |
| ★ buttons in top bar | filter ≥N stars / unrated |

Zoom framing persists across images — cull a burst at 100% comparing
the same detail. Viewr also restores the last window size and position.

The Preferences window controls the loading message, performance details,
exposure details, and cache indicator. The cache indicator has border, mark,
and hidden modes. All binds and the scroll behavior are also configurable.
Viewr stores these settings in
`~/Library/Application Support/viewr/viewr.toml` on macOS. Viewr writes a
documented template on the first run.

## Install

Download one of these files from
[Releases](https://github.com/hunterchen7/viewr/releases):

- macOS 11 or later, Apple Silicon: `viewr-macos-arm64.pkg`.
- Windows 10 or later, x64: `viewr-windows-x64.msi`.
- Ubuntu 22.04 or later, or Debian 12 or later, x64:
  `viewr-linux-x64.deb`.

Portable `.tar.gz` and `.zip` files remain available. The installers register
Viewr as an ARW viewer without overwriting an explicit default. On a macOS
account without an explicit ARW choice, Launch Services can infer Viewr as the
default while it is installed.

The current release packages are unsigned preview installers. They do not have
Apple Developer ID or Windows Authenticode signatures. macOS Gatekeeper or
Windows SmartScreen can show a security prompt. Verify the downloaded artifact
with its GitHub provenance attestation and `SHA256SUMS` before installation.

To build from source, run:

```
cargo build --release        # binary at target/release/viewr
cargo test --workspace
cargo doc --workspace --no-deps  # API and architecture contracts
```

Rust 1.96 is pinned; the application stack uses egui/wgpu → Metal on macOS,
DX12/Vulkan on Windows, and Vulkan on Linux. The bundled SQLite library is
native C. CI builds and tests all three platforms.
Linux builds need `build-essential pkg-config libgtk-3-dev
libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
libxkbcommon-dev`. RAW decoding by
[rawler](https://github.com/dnglab/dnglab) — Sony ARW first-class
including lossless compressed; DNG comes along for free.

See [installer and release procedures](docs/installers-and-releases.md) for
package validation, default-viewer steps, and release architecture.

## Notes

- Ratings precedence on load: an unfinished dirty DB journal entry remains
  authoritative until it is flushed; otherwise a current sidecar beats the
  clean local DB. In-camera ratings arrive later through the metadata wave and
  fill only remaining gaps. Sidecar writes are debounced, atomic, and
  merge-preserving. Existing Lightroom settings and keywords are semantically
  preserved; element updates or property injection can reserialize lexical XML.
- The local DB lives at `~/Library/Application Support/viewr/viewr.db`
  (the platform configuration directory elsewhere). The storage location must
  support SQLite WAL mode and local file locking. A network-backed or roaming
  profile that declines WAL leaves rating writes queued instead of publishing
  unjournaled XMP. On Windows, the current platform directory is Roaming
  AppData, so managed profiles must provide WAL-capable storage there.
- Current rating ownership resolves ordinary parent symlinks and
  filesystem-verified case and Unicode aliases. Linux bind mounts, or unusual
  case-folded mount spellings that canonicalize as distinct paths, cannot be
  proven equivalent. Use one mount spelling for a photo folder.
- Before the first 0.2.x launch, close every older Viewr process that can write
  the same photo folders. Mixed 0.1.x/0.2.x writers are not supported: a 0.1.x
  process can still replace an XMP file after the new database rejects its
  obsolete journal write. Do not relaunch or downgrade to 0.1.x for folders or
  databases already used by 0.2.x.
- [Testing and benchmark procedures](docs/testing-and-benchmarking.md).
- [Performance and adversarial audit](docs/performance-adversarial-pass-2026-07-21.md).
- [Design and implementation notes](docs/m0-notes.md).

## License

viewr is MIT-licensed. It links [rawler](https://github.com/dnglab/dnglab)
(LGPL-2.1) for raw decoding. Each package contains the exact rawler license,
the complete generated dependency-license inventory, Rust standard-library
notices, and source-build instructions. Each release contains a
version-matched source archive with vendored dependencies and a tested local
rawler replacement workflow.

## Releases

Automated with release-please: commits to `main` using
[Conventional Commits](https://www.conventionalcommits.org) (`feat:`,
`fix:`, …) accumulate into a release PR; merging it tags a version,
generates the changelog, and CI attaches archives and native installers.
