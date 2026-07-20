# viewr

A fast RAW culling viewer. Built for burst photoshoots: flip through
hundreds of Sony ARWs with near-zero latency, zoom into the *actual
decoded raw* (never the embedded preview JPEG) to judge focus, rate
keepers 1–5 stars, then import into Lightroom and filter by rating —
the ratings live in Lightroom-compatible `.xmp` sidecars.

## Speed model

Everything is decoded ahead of where you're looking, in an outward wave
from the current image with a ~3:1 forward bias, into concentric cache
rings — all byte-budgeted:

1. **GPU textures** (current ±2) — drawn directly.
2. **Decoded RGBA in RAM** — instant display.
3. **JPEG bytes in RAM** — memoized develops, ~20× smaller, cheap re-inflate.
4. **Disk** (`~/Library/Caches/viewr/`) — folder reopens are instant;
   never written inside photo folders.

Two develop tiers, both from real raw data: half-res superpixel for
browsing, full-res PPG demosaic for 100% zoom (progressive swap).

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
| wheel / pinch, drag | zoom, pan |
| G (Enter in grid) | grid ↔ loupe |
| I | metadata panel |
| F | fullscreen |
| Cmd+O | open folder |
| ★ buttons in top bar | filter ≥N stars / unrated |

## Build

```
cargo build --release        # binary at target/release/viewr
cargo test --workspace
```

Rust stable, macOS-first (pure Rust; egui/wgpu → Metal). RAW decoding by
[rawler](https://github.com/dnglab/dnglab) (LGPL-2.1) — Sony ARW
first-class including lossless compressed; DNG comes along for free.

## Notes

- Ratings precedence on load: sidecar > local DB > in-camera rating.
  Sidecar writes are debounced, atomic, and merge-preserving (existing
  Lightroom develop settings/keywords in a sidecar are untouched).
- The local DB lives at `~/Library/Application Support/viewr/viewr.db`.
- Design/implementation notes: `docs/m0-notes.md`.
