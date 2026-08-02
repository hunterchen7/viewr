# Browsing and loading optimization pass — 2026-08-01

This pass targets image browsing and loading latency only. Every accepted
change keeps observable behavior identical: same pixels, same plans, same
policies, same errors. Changes that could alter output bytes were rejected or
gated behind exact-equality tests.

## Method and measurement conditions

Hardware: Apple M5 MacBook Pro, 10 cores, 24 GB, macOS 26.2, Rust 1.96.1.
Fixture: private Sony 33 MP ARW (`HCA04875.ARW`) for real-RAW cases.

The machine ran a concurrent unrelated build agent during much of this pass
(load average 11–31). Identical binaries varied by more than 40% between
Criterion runs in the worst windows. Every verdict below therefore comes from
a paired comparison: a `--save-baseline` immediately before the change and a
`--baseline` comparison immediately after, with unchanged-code control cases
used to detect environmental noise. Numbers from noisy windows are labeled.

## Accepted changes

### Bulk-copy opaque RGBA texture conversion

Texture upload converted `PixelBuf` RGBA to `egui::Color32` through
`ColorImage::from_rgba_unmultiplied`, a branchy per-pixel loop, on the UI
thread — for every Browse/Full/thumbnail upload and every progressive Full
tile. Decoded photo pixels are opaque, and for alpha 255 that conversion is a
byte-for-byte copy. The shared `pixels` module now scans alpha per
4096-pixel chunk and bulk-copies opaque chunks through a safe `bytemuck`
cast; a chunk containing translucency keeps the exact per-pixel conversion.
Unit tests pin byte equality against egui's own conversion across chunk
boundaries and alpha classes.

Measured (noisy window; opaque cases vs the per-pixel path in-session): four
visible 1024-px tiles 2.51 ms vs 7.2–9.3 ms; whole 24 MP frame 15.9 ms vs
25.5 ms. New `*_opaque` bench cases pin the production path.

### Per-thread resizer reuse

`resize_exact` constructed a fresh `fir::Resizer` per call, reallocating and
first-touch-faulting the multi-megabyte convolution intermediate on every
downscale. A thread-local resizer keeps the scratch buffers warm. Scratch
state is the only state a `Resizer` retains, so output is unchanged.

Measured (noisy window, paired): 12.2 MP → 2048 px −29% (26.2 → 20.1 ms
median), → 360 px −36% (22.3 → 15.7 ms), 1920×1280 exact −22%.

### RGB-native preview downscale for thumbnails

`thumb_and_meta` expanded the full embedded preview to RGBA before
downscaling four channels to the 360 px thumbnail. Camera previews decode as
RGB8, so `downscale_rgb8_to_rgba_fit` now convolves the native three-channel
layout and expands only the small result. A resize test pins exact byte
equality between the RGB and expand-first paths; `fast_image_resize`
convolves channels independently and a constant opaque alpha plane resizes
to itself, so output is unchanged. Non-RGB8 layouts keep the previous path.

Measured (noisy window, paired): real 33 MP ARW `thumb_and_meta_360` −24%
median (9.73 → 8.62 ms).

### Single-lock navigation replan probes

`Engine::navigate` probed the RAM cache up to twice per plan target (plus a
persistence-set lock per resident Full) on the UI thread — hundreds of mutex
round trips per keypress under the adaptive Full wave, contending with
worker publishes. `RamCache::probe_residency` classifies the whole target
list under one lock, and the persistence set locks once per replan. Plans
are advisory (workers re-check at claim), so the installed job set is
unchanged for identical cache states.

Measured (quiet window): 200-target classification 2.53 µs batched vs
4.70 µs individual (−46%) with zero contention; the contended case
additionally removes 399 lock handoffs per replan.

### Single-lock tier-indicator probes

With the border or marks indicator active, each visible grid cell took the
cache mutex up to eight times per painted frame. `RamCache::image_residency`
reports (Full RGBA, Browse RGBA, any JPEG) under one lock; `cache_state` is
now exactly one acquisition. A unit test pins the batched answers against
the individual probes.

### XMP rating prefilter

XML cannot escape element or attribute names, so any semantic rating
contains the literal bytes `Rating`. A `memmem` prefilter returns
`parse_rating`'s `None` without constructing the namespace-aware reader for
rating-free sidecars. Rated documents parse exactly as before.

Measured (quiet window): rating-free realistic sidecar 265.7 ns vs 17.1 µs
(~64×); rated documents unchanged.

### Develop transfer-table warm-up

The 65,536-entry gamma/tone LUT was built inside the first develop's timed
gamma stage on every launch. A short-lived startup thread now builds it
before the first develop needs it. Same table, same `OnceLock`.

### Allocation-free folder-scan extension filter

Every directory entry paid two allocations (lossy conversion plus Unicode
lowercase) for the arw/dng test, and built its display name before that
filter ran. `OsStr::eq_ignore_ascii_case` accepts the identical set — no
non-ASCII sequence lowercases to `arw` or `dng` — with no allocation, and
the name String is built only for RAW entries. The final sort is now
`sort_unstable_by`; names are unique within a directory, so order is
unchanged.

### Banded-rotation exactness test (coverage only)

The Rayon-banded quarter-turn path had no exact-output test; only small
serial-path images were covered. A 331×203 noise image with partial final
bands now pins both quarter turns byte-for-byte against the serial
reference.

## Rejected in this pass

- **Restructured rotation band loop.** Pixel-array casts with contiguous
  source runs were prototyped against the new exactness test. Paired runs
  on the shared machine could not demonstrate a win — the 33 MP cases
  trended neutral-to-worse against the quiet baseline — so the loop stays
  unchanged.

- **`fast_image_resize/rayon` parallel convolution.** Byte-identical output
  and a likely multi-ms win per downscale, but the feature transitively
  enables `image/rayon`, which pulls `rav1e`, `ravif`, `exr`, and ~30 more
  build dependencies into the tree. Disproportionate build, license-manifest,
  and audit surface for the win. Revisit only if upstream decouples the
  feature.
- **Uninitialized rotation/decode output buffers.** `vec![0u8; n]` for large
  buffers is `calloc`, which maps lazily-zeroed pages — there is no memset to
  remove, so the `MaybeUninit` pattern would add unsafe surface for no
  measurable gain.
- **Per-frame progressive-tile order caching.** The priority sort covers at
  most ~70 tiles; microseconds per frame. Not worth the invalidation state.
- **Skipping identical `set_thumbnail_demand` calls.** The engine's urgent
  queue may rely on the per-frame re-push for retry recovery; memoizing
  identical sets risks stalling thumbnail retries. Needs a liveness audit
  first.

## Standing rejections that still hold

Calibrate+pack fusion (tried twice, slower), jpeg-rusturbo as decoder,
one-chunk-per-worker JPEG split, PPG port into this repository (LGPL/MIT
mixing — upstream-first), Browse↔Full decoded-RAW sharing (revisit note
below), Rayon worker caps, batched SQLite `IN` rating queries.

## Remaining leads

1. **PPG demosaic upstream.** Full develop on 33 MP is still dominated by
   rawler's PPG (~186 ms total develop). rawler expands a zero-filled RGB
   frame, then runs four full-frame passes; passes three and four are
   fusable and the border pass is serial. The route stays: measured
   optimization in rawler, then a pinned update with exact pixel comparisons.
2. **Decoded-RAW sharing between Browse and Full.** The prior rejection was
   measured on a 20 MP fixture (~7.3 ms decode). The 33 MP corpus decodes in
   ~61 ms, so the duplicate entropy decode now costs ~10× the number the
   rejection was based on. Worth re-evaluating with current cancellation
   constraints.
3. **Metadata-only rawler source without whole-file readahead** (upstream
   API) for the folder-open metadata wave.
4. **Persistence encode/write pipelining** for warm-lane throughput.

## Verification

- `cargo fmt --all --check`, Clippy `-D warnings` (workspace, all targets,
  all features), full workspace test suite, and the Rustdoc gate.
- Ignored `real_sony_raw_` suites run against all four private corpus files,
  including the portrait-orientation fixture.
- New unit tests pin: Color32 conversion byte equality, RGB-vs-RGBA
  downscale byte equality, banded-rotation exactness against the serial
  reference, batched-vs-individual cache probes, and the XMP prefilter.
