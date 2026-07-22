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

1. **GPU textures** — Browse for the current ±2 visible images, Full only for
   the current zoomed image, plus viewport thumbnails in an LRU accounting for
   at most 256 MiB of logical RGBA bytes (actual backend allocation can differ;
   at most eight new thumbnails upload per frame).
2. **Decoded RGBA in RAM** — exact LRU with a target byte budget for instant
   display. Pinned current and nearby images can keep a ring above its target
   until they are unpinned.
3. **JPEG bytes in RAM** — memoized develops with a target byte budget, ~20×
   smaller and cheap to re-inflate. Pins have the same soft-budget behavior.
4. **Disk** (`~/Library/Caches/viewr/`) — a target budget for developed JPEGs
   used on fast folder reopens; cache GC enforces the target rather than every
   write, and files are never written inside photo folders.

Both develop tiers use real RAW data: half-res superpixel for browsing and
full-res PPG demosaic for 100% zoom. Fit mode does not schedule, pin, or upload
Full renders. The main view can show its demanded embedded thumbnail while a
RAW-derived Browse render is still in flight.

## Usage

```
viewr <folder|file.arw>        browse a folder of raws
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
| ★ buttons in top bar | filter ≥N stars / unrated |

Zoom framing persists across images — cull a burst at 100% comparing
the same detail. All binds and the scroll behavior are configurable in
`~/Library/Application Support/viewr/viewr.toml` (a documented template
is written on first run).

## Install

Grab a binary from [Releases](https://github.com/hunterchen7/viewr/releases)
(macOS Apple Silicon, Windows x64, Linux x64), or build from source:

```
cargo build --release        # binary at target/release/viewr
cargo test --workspace
cargo doc --workspace --no-deps  # API and architecture contracts
```

Rust 1.96 is pinned; the application stack uses egui/wgpu → Metal on macOS,
DX12/Vulkan on Windows, and Vulkan on Linux. The bundled SQLite library is
native C. CI builds and tests all three platforms.
Linux builds need `libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev
libxcb-xfixes0-dev libxkbcommon-dev`. RAW decoding by
[rawler](https://github.com/dnglab/dnglab) — Sony ARW first-class
including lossless compressed; DNG comes along for free.

## Notes

- Ratings precedence on load: an unfinished dirty DB journal entry remains
  authoritative until it is flushed; otherwise a current sidecar beats the
  clean local DB. In-camera ratings arrive later through the metadata wave and
  fill only remaining gaps. Sidecar writes are debounced, atomic, and
  merge-preserving. Existing Lightroom settings and keywords are semantically
  preserved; element updates or property injection can reserialize lexical XML.
- The local DB lives at `~/Library/Application Support/viewr/viewr.db`
  (platform-equivalent config dir elsewhere).
- [Testing and benchmark procedures](docs/testing-and-benchmarking.md).
- [Performance and adversarial audit](docs/performance-adversarial-pass-2026-07-21.md).
- [Design and implementation notes](docs/m0-notes.md).

## License

viewr is MIT-licensed. It links [rawler](https://github.com/dnglab/dnglab)
(LGPL-2.1) for raw decoding; distributed binaries include that LGPL
component, whose source and license are available at the link above.

## Releases

Automated with release-please: commits to `main` using
[Conventional Commits](https://www.conventionalcommits.org) (`feat:`,
`fix:`, …) accumulate into a release PR; merging it tags a version,
generates the changelog, and CI attaches macOS, Windows, and Linux binaries.
