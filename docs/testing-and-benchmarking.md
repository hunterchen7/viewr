# Testing and benchmarking

This guide defines the local quality checks and the performance test method.
Use Rust 1.96, which `rust-toolchain.toml` selects.

## Quality checks

Run these commands before each commit:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
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

CI runs the full test suite on macOS, Windows, and Linux. Platform-independent
formatting, Clippy, Rustdoc, benchmark compilation, and release compilation run
once on Linux. Miri uses one pinned Linux nightly job. The independent quality,
test, optimized-build, and Miri jobs run in parallel.

The optimized-build job builds the application and both benchmark harnesses
with one release-profile Cargo invocation using
`--bins --benches --all-features`. It then runs the complete workspace test
suite in release mode and executes both Criterion harnesses in smoke-test mode.
These checks are intentional: optimized-only parallel paths, code hidden behind
`debug_assert!`, and benchmark runtime setup must execute in CI, not merely
compile.

CI caches downloaded Cargo registry and Git sources plus compiled dependency
artifacts. Quality, test, optimized-build, benchmark, and Miri jobs use separate
keys because their toolchains and profiles are not interchangeable. Pull
request jobs can restore default-branch caches, but only `main` writes new
caches. This avoids filling the repository cache quota with merge-ref caches
that cannot seed `main`. Miri caches sources only because its focused checks are
not on the critical path.

Workspace crate outputs are intentionally excluded from the shared cache.
Fresh hosted checkouts can invalidate local-source artifacts by modification
time, and release/LTO outputs are large. Add a compiler cache or workspace-crate
cache only after a hosted cold/warm comparison shows a net benefit. A July 2026
hosted comparison rejected `sccache`: its fully warm build had a 100% cache-hit
rate but took 10m34, only eight seconds less than the Cargo target cache's
10m42 warm build, while its cold build took 17m11 and created hundreds of cache
objects. The optimized-build job therefore retains the simpler Cargo target
cache.

Normal test builds exclude Criterion and use an unoptimized test profile. This
keeps correctness feedback fast without changing the optimized development,
benchmark, or release profiles. The `benchmarks` feature opts into Criterion and
the custom benchmark targets.

## Synthetic benchmarks

Run the full benchmark suite:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked
cargo bench -p viewr --features benchmarks --bench filmstrip_scaling --locked
```

Use a filter when you work on one subsystem:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- navigation_plan
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- jpeg
```

The suite measures these workloads:

- Navigation planning and production priority-queue synchronization for 100,
  1,000, and 10,000 images, plus the former folder-wide planner as an
  explicitly labeled reference. The queue benchmark excludes decoder threads
  and filesystem cache probes.
- Folder-open metadata queue construction for up to 100,000 entries.
- Warm, in-memory, all-hit SQLite rating lookup for up to 50,000 entries.
  This case excludes database-open and disk-read costs.
- Outward-order construction for up to 1,000,000 images.
- Resize of a deterministic 12.2-megapixel image, plus rotation at 12.2 and
  32.7 megapixels.
- JPEG encoding and decoding at the production Browse and Full dimensions and
  qualities, plus RAM-cache hits and eviction scaling.
- XMP parsing, XMP updates, and disk-cache key generation.
- Warm, under-budget cache-GC scans for up to 10,000 objects.
  This case does not sort or delete cache objects.
- Loupe filmstrip widget scaling at 10,000 and 50,000 images.
- Thumbnail texture-LRU maintenance for 200 touches among 773 residents.

Criterion stores reports in `target/criterion`.
Git ignores this directory.
See [the first reference run](benchmark-baseline-2026-07-21.md).
See [the optimization campaign](performance-optimization-2026-07-21.md) for the current results and tradeoffs.
See [the second performance and adversarial pass](performance-adversarial-pass-2026-07-21.md)
for UI scaling, cold-thumbnail probes, native sampling, and Miri coverage.

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

## Real RAW benchmarks

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
