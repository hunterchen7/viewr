# Progressive Full-texture experiment — 2026-07-26

## Objective and correctness gate

Reduce the time from entering a zoomed view to having Full-resolution pixels
under the viewport. Preserve zoom geometry, exact source pixels, linear
filtering across tile boundaries, and the existing Browse fallback. Do not
change UI controls or chrome.

The experiment starts after Full RAW development has completed. Rawler's PPG
demosaic remains a whole-render operation; splitting that stage would require a
separate decoded-mosaic cache, overlap-aware demosaic regions, and a larger
scheduler redesign.

## Variants

The durable baseline converts the complete 6000 by 4000 RGBA buffer to one
`egui::ColorImage`. Three variants convert only the tiles intersecting a
centered 1500 by 950 viewport at 100% zoom:

| Variant | Visible tiles | Median CPU preparation |
|---|---:|---:|
| Complete 24 MP texture | 1 | 22.843 ms |
| 512-pixel tiles | 12 | 3.794 ms |
| 1024-pixel tiles | 4 | 4.507 ms |
| 2048-pixel tiles | 2 | 8.406 ms |

Command:

```sh
cargo bench -p viewr --features benchmarks \
  --bench filmstrip_scaling -- full_texture_first_visible --noplot
```

Environment: base commit `b936d69`, Rust 1.96.1, macOS arm64, Darwin 25.2.0.
These measurements include CPU-side pixel conversion and allocation. They do
not measure backend GPU transfer or rendering.

## Decision

Promote 1024-pixel tiles. The first four tiles cover the representative
viewport in one frame and prepare about five times faster than the complete
texture. The 512-pixel variant has the lowest aggregate CPU time but needs
twelve texture uploads and three frames under the four-visible-tiles budget.
The 2048-pixel variant halves the texture count but more than doubles initial
CPU preparation relative to 512-pixel tiles and moves larger transfers into
one frame.

The Browse texture remains underneath throughout. Full tiles intersecting the
current viewport upload first; after that viewport is complete, one tile per
frame expands outward. A one-pixel sampling gutter prevents linear-filtering
seams. The loading badge reflects only missing visible Full tiles, so
background expansion does not keep the badge on screen.
