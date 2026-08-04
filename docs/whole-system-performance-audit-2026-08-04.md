# Whole-system performance, correctness, and architecture audit

Date: 2026-08-04

Audited base: `e27a133c4183258f35a241eded4540c1d7f7ddc3`

Audit branches:

- Viewr: `codex/whole-system-performance-audit`
- Rawler fork: `codex/whole-system-performance-audit`

## Scope and product invariant

This pass examines every measured operation that can take more than a few
milliseconds. It covers the application frame path, RAW decode and develop,
JPEG persistence and rehydration, scheduling, cache ownership, settings and
rating persistence, benchmark tools, release source archives, and unsafe code
at the modified boundaries.

The product invariant is strict: this work must not change the user interface.
It can change when data becomes ready. It must not change widgets, layout,
labels, colors, controls, image bytes, rating precedence, or input behavior.

The audit used independent work streams for these areas:

- RAW decode, demosaic, bounds, memory, and unsafe access.
- JPEG encode/decode libraries, thread topology, memory, and packaging.
- Scheduler, progressive publication, cancellation, and cache ownership.
- Application-frame work, settings durability, ratings, and texture preparation.
- Adversarial review, malformed inputs, benchmark validity, and CI coverage.

## Measurement host and fixture

The local reference host has this configuration:

- Apple M5 with 10 logical CPU cores.
- 24 GiB memory.
- ARM64 macOS 26.2.
- Rust 1.96.1 and LLVM 22.1.2.
- Thin LTO in release and benchmark profiles.

The private RAW fixture is not tracked by Git:

- File: `HCA04875.ARW`
- Size: 36,237,312 bytes.
- SHA-256:
  `a649f862d6d5e9ec5b78bcc67cdd9860efe8ace776184f754b8703bcf78fa0f6`
- Camera: Sony ILCE-7CM2.
- Sensor container: 7168 by 5120.
- Full display output: 7008 by 4672.

Criterion comparisons used an unchanged fixture, profile, and host. Candidate
tests used alternating order or named baselines when practical. A change was
kept only when it preserved exact output and produced a useful end-to-end win.
A kernel-only improvement was not sufficient.

## Initial latency map

These values are the isolated reference measurements collected before this
pass. They are local scale evidence, not portable limits.

| Surface | Workload | Reference time |
| --- | --- | ---: |
| RAW container | Metadata only | 0.064 ms |
| RAW preview | Thumbnail and metadata | 5.169 ms |
| RAW entropy | Full Sony mosaic decode | 46.729 ms |
| RAW copy | 33 MP `u16` mosaic clone | 3.422 ms |
| Develop | Browse tier | 13.147 ms |
| Develop | Full tier | 125.320 ms |
| Resize | RGBA long edge to 2048 | 14.906 ms |
| Resize | RGBA long edge to 360 | 9.642 ms |
| Resize | Exact production shape | 13.388 ms |
| Orientation | 33 MP quarter turn | 8.300 ms |
| JPEG encode | Browse, quality 97, 4:4:4 | 28.338 ms |
| JPEG encode | Full, quality 97, 4:4:4 | 114.100 ms |
| JPEG decode | Browse, parallel restart rows | 9.734 ms |
| JPEG decode | Full, parallel restart rows | 37.392 ms |
| Rating groups | 100,000 owner entries | 12.095 ms |
| Filmstrip reference | Construct all 10,000 widgets | 7.835 ms |
| Filmstrip reference | Construct all 50,000 widgets | 44.592 ms |
| Filmstrip production | Construct only the viewport | about 0.015 ms |
| Full texture prep | One 24 MP RGBA image | 21.342 ms reference; 6.967 ms optimized opaque path |
| Full tile prep | Four visible opaque tiles | 1.901 ms |

The table separates work that blocks an interaction from work that already
runs in the background. It also exposes misleading totals. For example, the
old progressive callback could report a developed region before one complete
guttered GPU tile was available. That callback time was not time to a sharp
pixel on screen.

## Accepted changes

### Reproducible pipeline records

`viewr dev --json` now emits one machine-readable record with stage times,
input identity, dimensions, CPU configuration, cache conditions, output sizes,
and SHA-256 values for both RGBA and JPEG results. Hashing runs after the timed
pipeline. The human diagnostic output remains available.

This record closes a benchmark architecture gap: a fast run can no longer hide
a different input or different output bytes.

### RAW pixel and entropy bounds

Rawler pixel access now checks complete coordinates before it computes a flat
index. Truncated JPEG marker reads and byte-stream marker skips also fail
without reading one byte past the source.

The lossless-JPEG structural parser is now fallible at every byte boundary.
SOF, SOS, DHT, APP, and COM segments are borrowed at their declared length and
parsed only inside that slice. It rejects short or overflowing lengths,
duplicate or inconsistent components, unsupported scan fields and restart
markers, invalid predictor and point-transform combinations, missing tables,
and invalid sampling geometry. Huffman code lengths are checked for empty or
oversubscribed code space before any decode table is allocated, and lossless
difference categories are limited to the supported 16-bit range. A prefix
test reparses every truncation of a generated stream under `catch_unwind`; the
structural prefix also passes strict-provenance Miri. Valid Sony output is
unchanged.

The Sony tiled-LJPEG path now validates all of these conditions before it
starts parallel work:

- Nonzero image and tile dimensions.
- The complete 512 by 512 Sony tile grid used by this codec path.
- Checked output-size and tile-count arithmetic.
- Matching `TileOffsets`, `TileByteCounts`, and expected grid cardinality.
- Nonzero, non-overflowing byte ranges that remain inside the mapped file.

Each decoder receives the exact byte slice for one tile. A truncated tile can
no longer consume entropy bytes from its neighbor. The checked grid also
proves that parallel output rectangles are disjoint.

### Sony tile decoder setup

Every Sony tile has the same LJPEG table shape. The decoder now builds one
validated template and shares the immutable Huffman tables instead of parsing
and allocating them for all 140 tiles.

An alternating real-RAW comparison changed the decode median from 32.856 ms to
30.274 ms, a 6.08 percent improvement. The two-sided paired result used by the
optimization loop was significant at `p = 0.01`.

### Sony in-place LJPEG expansion and tiled-output provenance

The Sony decoder formerly expanded each 256 by 256, four-component LJPEG tile
into a 512 KiB packed buffer and then scattered that buffer into a 512 by 512
Bayer rectangle. It now decodes predictor samples directly into the two Bayer
rows that own each component. A checked strided-row wrapper is the only unsafe
boundary. It retains the complete allocation provenance, validates the final
row end, and requires the caller to prove that parallel tile rectangles are
exclusive.

The same review found an older provenance defect in Rawler's general mutable
tile iterator: later row pointers were derived from the first row's slice
instead of the complete allocation. The iterator now keeps the original
allocation pointer, validates the exact tile grid with checked arithmetic, and
documents the `Send` and exclusive-ownership contracts. Strict-provenance Miri
reproduced the old failure and passes the replacement.

The direct Sony path is enabled only for four 1 by 1 sampled components,
predictors 1 through 7, and sufficient source dimensions. Other headers use
the legacy decoder. Differential fixtures cover every predictor, cropped
sources, guarded row edges, subsampling rejection, and dummy decode. The real
RAW mosaic hash is unchanged.

The first exploratory 11-pair window was noisy: the candidate won 6 pairs and
changed the median from 29.439 ms to 28.685 ms. A second isolated 20-pair
window won 15 pairs and changed the median from 32.597 ms to 31.112 ms, a 4.55
percent improvement (`p = 0.041` for that confirmatory window). Across both
windows it won 21 of 31 pairs (`p = 0.071`), so the latency evidence is modest
rather than conclusive. Process peak RSS nevertheless fell from 160,415,744
to 149,618,688 bytes, and peak footprint fell about 9.2 percent. The exactness,
memory reduction, and isolated latency result justify keeping the scoped path.

### Regional PPG demosaic

The regional PPG path no longer performs full interior scans when it only
needs a halo around one requested rectangle. It retains the same boundary
math and exact pixel order.

The measured regional median changed from 26.362 ms to 24.369 ms, an 11.49
percent improvement (`p = 0.01`). The complete Full benchmark improved by
about 6 percent, and the isolated region stage improved by about 19.7 percent
in the final paired run.

The parallel implementation no longer creates aliased Rust references through
raw pointers. Component loads and stores use explicit raw-pointer operations.
Parallel execution is enabled only when the complete sampled CFA proves a true
2 by 2 periodic Bayer layout. Unusual layouts and small images use the
deterministic sequential path.

### Generic four-channel bilinear demosaic

The initial bounds hardening made every generic RGBE interpolation sample use
a complete checked `(row, column)` lookup. A controlled reconstruction of the
pre-hardening implementation showed that this was a real regression: the
single-thread median changed from 7.670 ms to 11.600 ms, and the checked path
lost all four counterbalanced pairs.

The retained implementation proves the three interior row bounds once and
then indexes safe row slices. Its output fingerprint is bit-exact with both
the pre-hardening and checked implementations. It measured 7.469 ms with one
thread, 35.6 percent faster than the checked implementation, and 2.863 ms
versus 3.819 ms in the noisier ten-thread lane. This optimization is limited
to the measured four-channel bilinear path. Similar-looking RW2, IIQ, and RADC
accesses remain unchanged without representative fixtures.

### Fused integer Browse development

The common Browse path no longer creates a complete normalized `f32` CFA and
then rereads it to form 2 by 2 superpixels. It normalizes the four integer
samples directly into each RGB superpixel. This removes a sensor-sized
allocation and one complete memory traversal.

Selection is deliberately narrow: integer data, one of the four shifted 2 by
2 Bayer patterns, an in-bounds active rectangle, finite levels, and a positive
finite white-minus-black range. Float mosaics, unusual CFAs, malformed levels,
and malformed geometry retain the materialized path. Absolute sensor parity,
shifted active areas, crop offsets, odd final rows and columns, and the exact
floating-point operation order have differential tests.

The first implementation exposed the cost of per-sample indexing: its
steady-state median was 12.855 ms, versus 11.284 ms for the materialized path,
despite a much faster cold run. The retained row-pair implementation slices
each input pair once and hoists odd-tail handling out of the normal loop. In a
15-pair release comparison it won 13 pairs and changed the median from 11.850
ms to 9.346 ms, a 21.13 percent improvement. Criterion measured 11.293 ms
materialized and 8.643 ms fused, a 23.47 percent median improvement with
non-overlapping confidence intervals. The real Sony Browse RGBA BLAKE3 remains
`dfdc98ef836738287087d209f35b7400d097132f6250c72c02c8c89ebfc7c527`.

### Progressive publication and first sharp tile

Progressive staging uses a generation-scoped RAII lease. Allocation and large
drops occur outside the publication lock. Poison recovery, cancellation,
session replacement, corrupt-cache invalidation, and stale generations have
explicit tests.

Navigation, viewport, and direction hints are published as one transaction.
A Fit-to-zoom change and a moved zoom viewport force a new progressive plan.
The texture source records whether a tile came from a finished image, staging,
or a provisional JPEG band, so invalidation removes the correct data.

Every staging reservation and provisional-band publication revalidates the
navigation generation, current path, viewport or exact view hint, zoom state,
and cancellation token while holding one documented lock order. Changing the
current image clears prior band ownership before any later publication can
observe it. A cancelled follower waiting on a shared RAW decode returns
cancelled instead of falling through into a duplicate full decode; a genuine
leader failure can still be retried.

The first region now snaps outward to the union of every visible 1024-pixel
tile sample rectangle, including its one-pixel sampling gutter. It clamps at
image edges and inverse-maps R0, R90, R180, and R270. Core and app code use one
set of tile constants.

Tests reproduce the old failure as zero of one visible tiles being uploadable
after the first event. The new path makes every visible tile uploadable after
that event. The real Sony progressive assembly remains byte-identical to the
monolithic develop.

### JPEG restart-row encoder

The selected `jpeg-rusturbo` source is vendored with upstream identity,
license files, a checksum, patch notes, and source-archive validation. The
benchmark workspace keeps all tested codecs and the production shape.

For Viewr's baseline RGBA, 4:4:4 stream with a restart interval of one MCU row,
each row is an independent entropy segment. The encoder now performs color
conversion, DCT, quantization, and entropy coding for these segments in
parallel. It joins them in row order, writes the final entropy stream once,
and emits the exact restart-marker sequence. All other JPEG shapes keep their
existing path.

The final 21-pair Full, quality-97 comparison measured:

| Path | Median | Output size |
| --- | ---: | ---: |
| Serial restart rows | 155.945 ms | 20,157,604 bytes |
| Parallel restart rows | 27.152 ms | 20,157,604 bytes |

The result is a 5.743 times speedup. All 21 parallel samples were faster; the
two-sided sign-test probability is below `0.000001`. Serial and parallel
outputs are byte-identical for qualities 1, 50, 97, and 100; partial MCUs;
tiny images; several worker counts; and restart-marker wraparound.

The codec review also removed three decoder allocations that exposed
uninitialized vector lengths to safe code. Progressive encoding now separates
the padded interleaved MCU grid needed by DC scans from the true component
block grid needed by AC scans. Odd 9 by 7 and 17 by 16 4:2:0 images round-trip
in both standard and optimized progressive modes, and an independently
generated libjpeg-turbo fixture is accepted by both implementations.

A 20-image process stress test measured this resource change:

| Path | Wall time | CPU time | Peak RSS | Output bytes |
| --- | ---: | ---: | ---: | ---: |
| Old stream without row restarts | 2.26 s | 3.75 s | 435,912,704 | 403,140,500 |
| Parallel row-restart stream | 0.61 s | 4.15 s | 199,540,736 | 403,152,080 |

The new path reduces wall time by about 73 percent and peak RSS by about 54
percent. It uses about 11 percent more total CPU and adds standard DRI/RST
overhead to the file. The latency and memory wins justify that tradeoff.

### UI-thread filesystem and rating work

Normal configuration changes already use a background writer. Closing
Preferences now enqueues a save and a FIFO durability barrier without waiting
for filesystem synchronization. Process exit still waits for durable
replacement. Tests prove that a later save cannot coalesce across the close
barrier.

The asynchronous rating refresh used to rebuild shared-owner membership when
the UI received the database result. The 100,000-entry operation measured
10.816 ms in the final local rerun. The existing refresh worker now builds the
groups and publishes ratings plus their matching membership map in one
message. Initial startup behavior and rating precedence are unchanged.

### Bounded, priority-aware application events

Engine results now use two FIFO lanes. Image completion, progressive regions,
band invalidation, and image failures use the foreground lane. Metadata and
metadata-bearing thumbnail results use the background lane, which preserves
their rating-precedence order. A frame handles at most 256 foreground and
4,096 background events, then explicitly requests another repaint if it
reaches either bound.

A synthetic 100,000-metadata backlog took a median 10.933 ms to drain without
a bound. The prioritized bounded frame took 0.280 ms and delivered a queued
foreground result first. Both lanes have ordering, disconnection, liveness,
and exact-boundary tests.

### Proof-carrying opaque texture conversion

`PixelBuf` now carries private immutable alpha provenance. Safe external
construction either leaves provenance unknown or scans and validates opaque
storage. Core RAW, RGB expansion, parallel JPEG, resize, orientation, clone,
and band producers can preserve a proof but cannot upgrade unknown input.
Serial decoder output is scanned on its worker because some truncated streams
can return zero alpha.

The UI can therefore copy proven opaque bytes directly into `Color32` storage
without a second full-frame alpha scan. Unknown, translucent, and malformed
storage retains the exact egui conversion. On the reference host, known-opaque
24 MP texture preparation measured 2.257 ms versus 6.910 ms for the same bytes
with unknown provenance, about 67 percent faster. Four visible tiles measured
0.924 ms versus 1.765 ms, about 48 percent faster. Translucent fallback output
remains byte-identical.

## Rejected or refined candidates

Rejected variants remain documented because they prevent the same attractive
but ineffective changes from being repeated.

| Candidate | Result | Decision |
| --- | --- | --- |
| Cache Bayer color/parity lookup in PPG | Full develop regressed 7.49 percent | Reverted in its own Rawler commit. |
| Sony per-worker tile scratch | About 0.5 percent slower | Rejected. |
| Uninitialized Bayer expansion | About 4.7 percent faster, but `p = 0.17` | Rejected as inconclusive unsafe complexity. |
| Function-level Bayer specialization | About 22.9 percent slower | Rejected. |
| Crop-only regional PPG | About 0.7 percent faster, `p = 0.82` | Rejected as noise. |
| Fuse calibration and final pack | Regressed in two designs | Rejected; keep cache-friendly separate passes. |
| Allocate one JPEG output per row | Peak RSS grew to about 580 MB | Replaced by caller-owned stripes and one final write. |
| ARM64 fused NEON RGBA JPEG front half | Exact tests passed; Full median moved from 27.15 to 27.75 ms | Removed because there was no end-to-end win. |
| Handwritten ARM64 assembly | No compiler-codegen defect remained after the intrinsics test | Not added. An unmeasured unsafe path is not an optimization. |
| Naive per-sample integer Browse fusion | Cold allocation improved, but steady-state changed from 11.284 to 12.855 ms | Refined into the accepted row-pair implementation. |

The assembly decision is deliberate. The tested NEON intrinsic already maps
to direct ARM instructions and keeps a scalar reference. Handwritten assembly
would add ABI, register-clobber, target-feature, and maintenance risk without
removing a measured bottleneck.

## Memory architecture

One 33 MP Full job can hold these principal allocations at different stages:

- About 70 MiB for the decoded `u16` CFA.
- About 140 MiB for the normalized `f32` CFA.
- About 374.7 MiB for a three-channel `f32` RGB frame.
- About 124.9 MiB for packed RGBA.

Lifetime shortening prevents all four values from being additive, but the
observed development peak remains about 515 MiB per large job. Multiple Full
workers can therefore create memory pressure outside the byte-bounded cache.

The long-term Full solution is a rolling source/green-line PPG pipeline with
direct calibrated packing. That design can approach a 200 MiB working set, but
it changes neighborhood ownership and floating-point order. It is not safe to
promote from one camera fixture. It requires exact tests on 24 MP, 33 MP, and
61 MP Bayer variants first.

Browse integer Bayer development no longer constructs the complete normalized
CFA. On the reference Sony sensor this removes about 140 MiB of transient
`f32` storage. Full PPG still requires its complete normalized mosaic and RGB
neighborhood state.

## Architecture decisions and remaining work

### Keep scheduler and presentation ownership separate

The core scheduler owns decode generations, cache admission, cancellation,
and progressive coverage. The app owns GPU texture lifetimes. The shared tile
geometry constants are a narrow contract between them. Moving texture handles
or egui types into the core would make tests and worker ownership worse.

### Do not hide foreground work behind background work

Current-image Full work has priority over prefetch and persistence. Browse
neighbor uploads use a separate per-frame allowance. The two-lane bounded
event receiver keeps image readiness and failures ahead of a large metadata
backlog without reordering metadata rating precedence.

### Keep portable release code

This pass does not set `target-cpu=native`. A binary tuned for this exact M5
can use instructions that are not valid on every supported release host. SIMD
paths remain architecture-gated and have scalar references.

### Residual operations above a few milliseconds

These operations are measured and intentionally unchanged in this pass:

- Full PPG development is still the largest CPU and memory stage. The rolling
  design needs a wider RAW corpus.
- A 33 MP mosaic clone costs about 3.4 ms when Browse and Full must consume
  independent mutable normalization inputs. Removing it requires a shared or
  fused two-tier develop design.
- Quarter-turn orientation costs about 8.3 ms for a monolithic 33 MP buffer.
  Progressive region packing already avoids that full-frame tail for the
  interactive zoom path. A NEON transpose is not justified until this becomes
  a foreground bottleneck again.
- A large folder scan performs one metadata operation per candidate on macOS.
  A bounded parallel scan or `getattrlistbulk` implementation needs cold and
  network-filesystem tests before adoption.
- Initial owner grouping remains synchronous at folder open. The optimized
  refresh path removes repeated UI work; moving initial grouping requires a
  session-ready state and end-to-end UI coverage.
- Main GPU texture residency does not have a direct byte budget. RAM cache
  accounting cannot report driver-side texture allocation.
- Update checks and downloads run on workers, but the app still polls a small
  local update event queue. No measured update-frame operation crossed the
  few-millisecond threshold.

`jobs.rs` and `app.rs` still contain several responsibilities. Splitting them
without an orchestration benchmark would change lock and event boundaries with
no performance proof. The next architectural extraction should separate job
policy, persistence, and worker execution behind the current `Engine` API.

## Benchmark architecture

The benchmark system now has these layers:

- Structured fresh-process RAW records with input and output hashes.
- Criterion microbenchmarks for queue, cache, database, resize, orientation,
  JPEG, rating groups, filmstrip virtualization, event backlogs, opaque and
  unknown texture preparation, and materialized-versus-fused Browse develop.
- A separately locked Rawler Criterion harness for lossless-JPEG kernels and
  generic four-channel bilinear demosaic. CI pins its `CRITERION_HOME` so the
  uploaded report path cannot depend on Cargo's working-directory behavior.
- Ignored real-RAW tests for exact monolithic/progressive assembly and stage
  latency.
- A 13-codec JPEG bakeoff with quality, size, PSNR, edge-delta, and sampling
  checks.
- An alternating restart-row comparison with exact stream equality.
- Process-level JPEG stress measurements for wall time, CPU, RSS, and bytes.
- CI benchmark smoke tests that execute optimized setup and one iteration for
  every application, Rawler, and JPEG-bakeoff harness.

The scheduled benchmark artifact includes the Viewr commit and lock hash, the
exact Rawler submodule commit and lock hash, the JPEG-bakeoff lock hash, Rust
version, OS, CPU, logical core count, memory, and Rayon configuration. This
prevents a report from being attributed to the wrong vendored decoder.

Known limits remain explicit:

- Criterion does not measure GPU driver upload or input-to-present latency.
- The private Sony file does not represent every camera, CFA, bit depth, or
  orientation.
- Fresh-process RAW records cannot control the operating-system page cache.
- CI smoke tests detect panics and no-op benchmarks; they do not enforce a
  numeric performance threshold across hosted machines.

## Safety validation

Modified unsafe boundaries have a safe wrapper, a narrow precondition, and a
reference test. Validation includes these checks:

- Miri with Tree Borrows for pixel bounds, Bayer/X-Trans/tiny PPG cases, Sony
  tile validation and direct strided rows, general tiled-output provenance,
  JPEG truncation, Browse vector initialization and parity tails, and threaded
  output equivalence where Miri can run it.
- CI runs strict-provenance Rawler tests for mutable tile ownership, PPG raw
  pointers, Sony output rectangles, direct LJPEG scatter, and every structural
  LJPEG header prefix. It force-selects scalar JPEG entropy code so Miri can
  inspect both pointer-bump writers without SIMD intrinsics.
- Scalar-forced JPEG tests on ARM64.
- Exact serial-versus-parallel JPEG stream tests.
- Exact monolithic-versus-progressive RAW output tests.
- Panic, poison, stale-generation, cancellation, and malformed-storage tests.

Miri reports known Crossbeam integer-pointer provenance diagnostics when Rayon
initializes its global worker structures. The tested Viewr code completes. A
run without `-Zmiri-ignore-leaks` ends on Crossbeam's global thread and leak
state, not on a Viewr allocation or pointer error.

## Reproduce the important checks

Keep the private RAW outside the repository. Set its path only for the command
that needs it.

```sh
cargo test --workspace --locked
cargo test --workspace --release --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo test --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
  --lib --release --locked
RUSTDOCFLAGS="-D warnings" cargo doc \
  --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
  --all-features --no-deps --locked

VIEWR_BENCH_RAW=/path/to/HCA04875.ARW \
  cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- raw_opt_in --noplot

VIEWR_TEST_RAW=/path/to/HCA04875.ARW \
  cargo test -p viewr-core --release --locked \
  real_sony_raw_progressive_develop_matches_monolithic_and_stages_regions \
  -- --ignored --nocapture

cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- jpeg --noplot
cargo bench -p viewr --features benchmarks \
  --bench filmstrip_scaling --locked -- --noplot
cargo bench -p viewr --features benchmarks \
  --bench event_backlog --locked -- --noplot
CRITERION_HOME=/tmp/viewr-criterion-rawler \
  cargo bench --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
  --bench perf --locked -- --noplot

cargo test --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
cargo run --release --manifest-path tools/jpeg-bakeoff/Cargo.toml \
  --locked -- restart-compare 10 21 97

scripts/validate-third-party-licenses.sh
scripts/validate-source-archive.sh
```

## User-interface audit

Application changes in this branch are restricted to worker publication,
durability scheduling, rating data preparation, progressive coverage, and
texture-data conversion. The review compares UI construction and paint output
against the base. No setting, label, control, shortcut, style, layout value, or
image result is added or changed.
