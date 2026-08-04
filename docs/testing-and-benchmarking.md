# Testing and benchmarking

This guide defines the local quality checks and the performance test method.
Use Rust 1.96, which `rust-toolchain.toml` selects.

## Quality checks

Run these commands before each commit:

```sh
cargo fmt --all --check
cargo fmt --manifest-path thirdparty/dnglab/rawler/Cargo.toml -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo test --manifest-path thirdparty/dnglab/rawler/Cargo.toml --lib --release --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
```

The test suite covers these areas:

- Cache budgets, eviction, pin changes, concurrency, and disk replacement.
- Folder scans, navigation waves, queue generations, and cancellation.
- JPEG cache round-trips, resize geometry, rotation, and tone-curve invariants.
- RAW crop layout, Bayer superpixels, transfer-table error, and persistence bounds.
- XMP preservation, rating precedence, cross-process ownership, native paths,
  SQLite reopen behavior, and durable flushes.
- Configuration parsing and loupe layout mathematics.

CI reports five checks. It uses one shared Linux quality check, one current
macOS compatibility check, and one installer check for each release platform.
The full test suite runs on current macOS, Windows, and Linux. The macOS and
Windows checks also compile all-feature benchmark targets. The quality check
runs formatting, Clippy, Rustdoc, the release build, release tests, vendored
Rawler release tests, pinned public-domain Sony RAW tests, every optimized
benchmark smoke test, and the focused Miri checks.

Windows-specific database tests recreate ordinary drive-path rows from the
released ownerless schemas and from the pre-v8 owner schema. They verify clean
fallback, dirty recovery to the canonical publication owner, exact
ordinary-to-verbatim prefix matching, mixed-history quarantine, rejection of
drive-root-relative spellings, raw drive-case path/owner tombstones, duplicate
component-equivalent keys, lossless non-Unicode round-tripping, and fail-closed
malformed native-path handling.

The quality check builds the application and its three benchmark harnesses with one
release-profile Cargo invocation using
`--bins --benches --all-features`. It then runs the complete workspace and
vendored Rawler test suites in release mode and executes every Criterion
harness, including the two separately locked benchmark workspaces, in
smoke-test mode.
These checks are intentional: optimized-only parallel paths, code hidden behind
`debug_assert!`, and benchmark runtime setup must execute in CI, not merely
compile.

CI caches downloaded Cargo registry and Git sources plus compiled dependency
artifacts. The quality check, current macOS check, three installer targets, and
manual benchmarks use separate keys because their platforms and profiles are
not interchangeable. Pull request jobs can restore default-branch caches, but
only `main` writes new caches. This avoids filling the repository cache quota
with merge-ref caches that cannot seed `main`. The Miri steps reuse the quality
job's downloaded sources but write nightly target artifacts to temporary
runner storage so they do not enter the stable target cache.

Workspace crate outputs are intentionally excluded from the shared cache.
Fresh hosted checkouts can invalidate local-source artifacts by modification
time, and release/LTO outputs are large. Add a compiler cache or workspace-crate
cache only after a hosted cold/warm comparison shows a net benefit. A July 2026
hosted comparison rejected `sccache`: its fully warm build had a 100% cache-hit
rate but took 10m34, only eight seconds less than the Cargo target cache's
10m42 warm build, while its cold build took 17m11 and created hundreds of cache
objects. The quality check therefore retains the simpler Cargo target cache.

Normal test builds exclude Criterion and use an unoptimized test profile. This
keeps correctness feedback fast without changing the optimized development,
benchmark, or release profiles. The `benchmarks` feature opts into Criterion and
the custom benchmark targets.

## Synthetic benchmarks

Run the full benchmark suite:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked
cargo bench -p viewr --features benchmarks --bench filmstrip_scaling --locked
cargo bench -p viewr --features benchmarks --bench event_backlog --locked
cargo bench --manifest-path thirdparty/dnglab/rawler/Cargo.toml --locked --bench perf
cargo bench --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked --bench encode
```

Use a filter when you work on one subsystem:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- navigation_plan
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- jpeg
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- full_cache_policy
```

The suite measures these workloads:

- Fixed and adaptive navigation planning for 100, 1,000, and 10,000 images.
  The suite also measures production queue synchronization and the prior fixed
  Full-window policy. Filtered cases separate the public defensive
  normalization path from the engine's normalize-once path. The queue
  benchmark excludes decoder threads and cache file probes.
- Folder-open metadata queue construction for up to 100,000 entries. Queue
  destruction is excluded from the timed interval.
- Warm, in-memory, all-hit SQLite point lookup and complete folder-startup
  rating hydration for up to 50,000 entries.
- Read-write and read-only reopen of current rating databases with 1,000,
  10,000, and 50,000 image and owner-ledger rows. These are warm filesystem
  measurements and verify that current-schema readiness does not scale with
  row count.
- Read-compatible legacy folder loads against 1,000, 10,000, and 50,000
  history rows, paired with a full-scan reference implementation. A separate
  stress group covers zero dirty rows, dense dirty rows, and repeated clean
  filename stems for both supported legacy schemas.
- Cold migration of copied 1,000- and 10,000-row templates from both released
  ownerless schemas: v0.1.0 without the journal column and v0.1.1 with sparse
  recoverable and quarantined dirty rows. Both corpora combine existing and
  missing RAW paths and begin in the persistent WAL mode used by those
  releases. Separate 1,000-row all-online clean corpora measure the successful
  owner-assignment path.
- Cold v7-to-v8 migration of copied 1,000- and 10,000-row unresolved database
  templates, plus a 1,000-row all-online owned template. The templates remove
  both v8 additions (`images.owner_key_version` and
  `rating_global_revision.ownerless_revision`) before timing. Template
  creation and copy setup are outside the timed routine.
- Rating journal updates against owner ledgers of up to 50,000 rows, plus
  indexed pending-sidecar scans with zero and one dirty row.
- Batched physical sidecar-owner discovery for up to 50,000 ordinary and
  Unicode filenames.
- Outward-order construction for up to 1,000,000 images.
- Resize of a deterministic 12.2-megapixel image, plus rotation at 12.2 and
  32.7 megapixels.
- JPEG encoding and decoding at the production Browse and Full dimensions and
  quality, plus RAM-cache hits and eviction scaling. The suite measures Full
  working-set replacement, constant-time size snapshots, copy-on-write
  observation updates with live snapshots, and release of large final owners.
  A 200-target residency classification compares the batched single-lock
  snapshot with the per-target probe reference it replaced.
  A dark-gradient regression compares production quality against the legacy
  Full-cache setting. Decode throughput uses compressed input bytes; latency
  remains the primary comparison. The `jpeg_decode_serial` and
  `jpeg_encode_plain` groups keep the whole-buffer serial decode and the
  markerless encode measurable beside the production restart-marker split.
- XMP parsing, XMP updates, and disk-cache key generation. A rating-free
  sidecar case measures the substring prefilter's early return.
- Warm, under-budget cache-GC scans for up to 10,000 objects.
  This case does not sort or delete cache objects.
- Loupe filmstrip widget scaling at 10,000 and 50,000 images.
- Full-resolution texture conversion for a 24-megapixel image, compared with
  visible-region-first conversion at 512-, 1024-, and 2048-pixel tile sizes.
  This measures CPU image preparation, not backend GPU transfer time.
  Paired opaque-source cases measure the production bulk-copy conversion;
  the translucent originals keep the exact per-pixel fallback measurable.
- Thumbnail texture-LRU maintenance for 200 touches among 773 residents.
- Shared-owner group construction and rating installation through a prefilled
  rating map at 1,000, 10,000, and 100,000 entries. The installation primitive
  runs a threshold-filter transition predicate; it does not include event,
  persistence, repaint, or full-session costs.
- A 100,000-item application event backlog, comparing unbounded FIFO drain
  with foreground-priority delivery plus one bounded 4,096-item background
  batch. This times receiver and metadata-map work; production rating,
  filtering, marker, and repaint effects remain correctness-tested but are not
  included in the synthetic latency claim.
- Rawler lossless-JPEG encode/copy kernels and a 2,048 by 1,366 four-channel
  bilinear demosaic. The latter keeps generic non-Sony pixel-access changes in
  the controlled benchmark surface.

Criterion stores reports in `target/criterion`.
Git ignores this directory.
See [the first reference run](benchmark-baseline-2026-07-21.md).
See [the optimization campaign](performance-optimization-2026-07-21.md) for the current results and tradeoffs.
See [the second performance and adversarial pass](performance-adversarial-pass-2026-07-21.md)
for UI scaling, cold-thumbnail probes, native sampling, and Miri coverage.
See [the progressive Full-texture experiment](progressive-full-textures-2026-07-26.md)
for the tile-size comparison and promotion decision.
See [the cache JPEG-quality experiment](jpeg-quality-2026-07-27.md) for the
dark-gradient quality, storage, and latency tradeoff.
See [the parallel cache-decode experiment](jpeg-parallel-decode-2026-07-28.md)
for the restart-marker split decode and its rejected variants.
See [the adaptive Full-prefetch experiment](adaptive-full-prefetch-2026-08-01.md)
for the RAM policy, scheduler limits, and local benchmark results.

## Unsafe image-path checks

CI pins a nightly toolchain and runs the repository-owned unsafe rotation and
superpixel initialization paths under Miri. Run the same focused checks with:

```sh
cargo +nightly-2026-07-21 miri test -p viewr-core --lib resize::tests::rotate_ --locked
cargo +nightly-2026-07-21 miri test -p viewr-core --lib \
  develop::tests::superpixel_output_initialization_is_valid_under_miri --locked
```

The filters are intentional. A broader image-path run reaches unsupported
third-party ARM NEON or Crossbeam/Rayon internals before it can test more
repository code.

## Compare a change

Save a baseline before you change the code:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- --save-baseline before
```

Compare the changed code with that baseline:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- --baseline before
```

Use the same machine and power mode for both runs.
Stop high-load background tasks before each run.
Repeat a suspected regression at least two times.

Use these initial review limits:

- Review a deterministic CPU change above 5 to 10 percent.
- Review an image-pipeline change above 10 to 15 percent.
- Do not reject a change from one hosted-runner result.

The manual `Benchmarks` workflow is advisory.
Download its Criterion artifact to inspect the complete report. The artifact
also contains `benchmark-metadata.json` with the commit, Cargo.lock hash, Rust
version, OS, CPU, logical-core count, memory, Rayon setting, cache condition,
and fixture state. Benchmark artifacts are retained for 90 days.
CI smoke-runs every benchmark case but does not compare Criterion estimates
with a stored numeric baseline, so performance regressions still require a
reviewer to run and interpret a controlled before/after comparison.

The released-v0.1 migration corpora sample existing RAW files at a fixed
stride and leave the rest unresolved. The v0.1.0 corpus covers the original
clean mirror, while v0.1.1 also exercises recovery and quarantine. This avoids
making every large benchmark row a filesystem object. Paired 1,000-row
all-online clean corpora measure filesystem owner discovery and successful
ledger creation. The cold v7 suite likewise retains 1,000- and 10,000-row
unresolved-removal cases and adds a 1,000-row online-owner rekey case.

The suite also does not yet benchmark contended sidecar publication: a durable
XMP write holds SQLite's immediate transaction, so a slow external volume can
delay unrelated rating writers.

## Real RAW benchmarks

CI downloads the public-domain Sony DSC-RX100 `DSC00838.ARW` fixture from
[raw.pixls.us](https://raw.pixls.us/) and verifies its SHA-256 digest
`579a485b5126a25cbd55cbd5dadfa7d09cf021c99cc7d4869f9e56e3f759390b`
before use. The focused ignored tests cover raw-pixel decode, embedded
thumbnail and metadata consistency, analytical-versus-table transfer output,
copied-versus-strided crop output, and exact parallel-versus-serial decode of
real cache JPEGs. The pinned fixture gives the decoder
and develop pipeline one real-camera compatibility gate without storing a
20 MB binary in Git.

Set `VIEWR_TEST_RAW` to run the same focused tests with another Sony RAW file:

```sh
VIEWR_TEST_RAW=/absolute/path/photo.ARW \
  cargo test -p viewr-core --release --locked real_sony_raw_ -- --ignored
```

Set `VIEWR_BENCH_RAW` to one ARW or DNG file.
The file must remain untracked. It can stay outside the repository or under
the ignored `testdata/` directory.

Run this command on macOS or Linux:

```sh
VIEWR_BENCH_RAW=/absolute/path/photo.ARW \
  cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- raw_opt_in
```

Run these commands in PowerShell on Windows:

```powershell
$env:VIEWR_BENCH_RAW = "C:\Photos\photo.ARW"
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- raw_opt_in
```

The RAW suite measures metadata-only extraction, thumbnail extraction, the
complete `decode::load` path, and both develop qualities. The decode benchmark
includes source construction, metadata extraction, and CFA mosaic decode.
It does not isolate entropy decoding.
The develop benchmark excludes decode setup.
The initial probe and Criterion warm-up normally make these warm operating-
system page-cache measurements. They are not Viewr RAM- or disk-cache hits.
When `VIEWR_BENCH_RAW` is unset, the harness prints an explicit skip message.
Synthetic success must not be interpreted as real-camera fixture coverage.

Use `viewr dev` in a new process for a cold-path inspection:

```sh
cargo run --release --locked -p viewr -- dev /absolute/path/photo.ARW
```

Use structured output for a repeatable comparison:

```sh
cargo build --release --locked -p viewr --bin viewr
target/release/viewr dev --json /absolute/path/photo.ARW /tmp/viewr-dev-output
```

The command writes one JSON object to standard output. The command writes the
Browse and Full JPEG files to the output directory.

The `pipeline_total_us` value stops after both JPEG files are written. The
command calculates correctness hashes after that point. Thus,
`audit_overhead_us` does not change the pipeline result.

The record includes these items:

- The input SHA-256 digest and byte count.
- The camera model and RAW dimensions.
- The available logical CPU count and the Rayon environment value.
- The decode, develop, encode, and write times in microseconds.
- The Browse and Full RGBA and JPEG SHA-256 digests.
- A cache-condition label.

Run each control and candidate at least three times. Alternate the control and
candidate runs when the system load is not stable. Compare only records that
have equal input and output digests.

This command starts a new Viewr process. It does not clear the operating-system
page cache. Record the first run separately from later runs.

The record uses performance result schema 1. Add a new schema version when a
field changes its meaning or unit.

Use a private corpus for camera coverage.
Include 24, 33, and 61-megapixel files when they are available.
Include compressed ARW, lossless ARW, DNG, landscape, and portrait files.
Record each file hash, camera model, dimensions, and codec.

Do not commit private photos.
The repository ignores `testdata/` for this reason.

## Result metadata

Record this information with a long-term baseline:

- The Git commit and Rust version.
- The operating system, CPU, memory, and power mode.
- The Rayon thread count.
- The fixture hash and image dimensions.
- The cold-cache or warm-cache condition.

Use fixed hardware for regression alerts.
Use hosted runners only for advisory comparisons.
