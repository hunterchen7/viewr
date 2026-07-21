# Testing and benchmarking

This guide defines the local quality checks and the performance test method.
Use Rust 1.96, which `rust-toolchain.toml` selects.

## Quality checks

Run these commands before each commit:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
```

The test suite covers these areas:

- Cache budgets, eviction, pin changes, concurrency, and disk replacement.
- Folder scans, navigation waves, queue generations, and cancellation.
- JPEG cache round-trips, resize geometry, rotation, and tone-curve invariants.
- XMP preservation, rating precedence, SQLite reopen behavior, and durable flushes.
- Configuration parsing and loupe layout mathematics.

CI runs the tests on macOS, Windows, and Linux.
CI also compiles the benchmark harness on Linux.

## Synthetic benchmarks

Run the full benchmark suite:

```sh
cargo bench -p viewr-core --bench core_hot_paths --locked
```

Use a filter when you work on one subsystem:

```sh
cargo bench -p viewr-core --bench core_hot_paths --locked -- navigation_plan
cargo bench -p viewr-core --bench core_hot_paths --locked -- jpeg
```

The suite measures these workloads:

- Navigation planning for 100, 1,000, and 10,000 images.
- Outward-order construction for up to 1,000,000 images.
- Resize and rotation of a deterministic 12.2-megapixel image.
- JPEG encoding, JPEG decoding, RAM-cache hits, and LRU churn.
- XMP parsing, XMP updates, and disk-cache key generation.

Criterion stores reports in `crates/viewr-core/target/criterion`.
Git ignores this directory.
See [the first reference run](benchmark-baseline-2026-07-21.md).

## Compare a change

Save a baseline before you change the code:

```sh
cargo bench -p viewr-core --bench core_hot_paths --locked -- --save-baseline before
```

Compare the changed code with that baseline:

```sh
cargo bench -p viewr-core --bench core_hot_paths --locked -- --baseline before
```

Use the same machine and power mode for both runs.
Stop high-load background tasks before each run.
Repeat a suspected regression at least two times.

Use these initial review limits:

- Review a deterministic CPU change above 5 to 10 percent.
- Review an image-pipeline change above 10 to 15 percent.
- Do not reject a change from one hosted-runner result.

The manual `Benchmarks` workflow is advisory.
Download its Criterion artifact to inspect the complete report.

## Real RAW benchmarks

Set `VIEWR_BENCH_RAW` to one ARW or DNG file.
The file stays outside the repository.

Run this command on macOS or Linux:

```sh
VIEWR_BENCH_RAW=/absolute/path/photo.ARW \
  cargo bench -p viewr-core --bench core_hot_paths --locked -- raw_opt_in
```

Run these commands in PowerShell on Windows:

```powershell
$env:VIEWR_BENCH_RAW = "C:\Photos\photo.ARW"
cargo bench -p viewr-core --bench core_hot_paths --locked -- raw_opt_in
```

The RAW suite measures thumbnail extraction, entropy decode, and both develop qualities.
The develop benchmark excludes decode setup.
Criterion warm-up makes these results warm-cache measurements.

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
