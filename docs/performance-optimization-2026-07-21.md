# Performance optimization report — 2026-07-21

This campaign added deeper tests, statistical benchmarks, and targeted performance changes.
The measured implementation commit is `a3578568ef3b236d085cec88e122cdd30acc539f`.
Commit `ee688a7` adds a zero-size rotation guard found during the final audit.
The work is on branch `codex/deep-testing-benchmarks`.

## Result summary

Lower time is better.
The table uses Criterion estimates from named baselines on the same computer.
It uses slope estimates when they are available.
Full RAW uses a mean estimate because that benchmark uses fixed iteration counts.
The RAW develop estimates are steady-state measurements after Criterion warm-up.

| Hot path | Before | After | Result |
|---|---:|---:|---:|
| Navigation plan, identity, 10,000 images | 33.87 µs | 91.90 ns | 369 times faster |
| Navigation plan, 10 percent filter, 10,000 images | 5.906 µs | 264.08 ns | 22.4 times faster |
| Navigation plan with disk-warm targets, 10,000 images | 43.51 µs | 9.223 µs | 4.72 times faster |
| Rotate 33 MP RGBA, 90 degrees | 45.27 ms | 10.39 ms | 4.36 times faster |
| Rotate 33 MP RGBA, 270 degrees | 51.04 ms | 11.57 ms | 4.41 times faster |
| RAM-cache eviction, 10,000 resident entries | 50.69 µs | 76.10 ns | 666 times faster |
| Sony ARW Browse develop | 59.79 ms | 17.51 ms | 3.41 times faster |
| Sony ARW Full develop | 290.87 ms | 185.89 ms | 1.56 times faster |

The Full RAW estimate is noisy.
Its measured time reduction is 36.1 percent, with a 95 percent change interval from 12.7 to 54.6 percent.
The after-run mean interval is 135 to 250 ms.
Repeat the Full result before using 1.56 times as a regression target.

The RAM-cache data structure now gives constant-time lookup and amortized constant-time eviction.
This change adds a small cost to a hit.
An RGBA hit changed from 14.88 ns to 21.20 ns.
A JPEG hit changed from 14.66 ns to 22.66 ns.
The added cost is 6.32 ns for RGBA and 8.00 ns for JPEG.

The isolated RAW run found no significant decode change.
Its decode estimate changed from 40.29 ms to 41.46 ms.
The thumbnail and metadata estimate changed from 5.58 ms to 6.77 ms.
Criterion classified the thumbnail result as a regression.
No thumbnail implementation changed in this campaign.
Repeat this result before a thumbnail change.

## Changes

### Navigation and scheduling

- The app invalidates its filtered image list only when filter input changes.
- Visible-position checks use binary search.
- Navigation no longer scans the full image list for normal identity order.
- Navigation no longer performs synchronous disk-cache probes.
- The scheduler builds a heap in one operation and reuses its allocation.
- Disk rehydrate checks run on workers.

The normal identity path is now constant with respect to library size for its bounded neighbor wave.
The filtered path uses a linear current-position lookup in the filtered sequence.
It performs a bounded neighbor scan after that lookup.
The disk-warm path still scans warm targets and remains linear.

### Rotation

Quarter-turn rotation divides destination rows into 16-row bands.
Rayon processes the bands in parallel.
The tests compare every output byte with the reference mapping.

### RAM cache

The cache now uses a key-to-slot map and an intrusive exact LRU list.
It reuses vacant arena slots.
It keeps the oldest unpinned entry available for eviction.
The tests compare 20,000 deterministic operations with a simple reference model.
They also check all internal list and map invariants.

### RAW development

Browse development now writes Bayer superpixels directly into one output allocation.
Tests show exact equality with the rawler implementation for all four Bayer patterns.

A 65,536-entry table replaces three analytical transfer-function calls for each output pixel.
Criterion warm-up excludes the first table construction.
The first-use construction cost is not measured.
On the real Sony fixture, every changed channel differed from the analytical result by one output level.
Browse changed 111,325 of 32,741,376 RGBA bytes.
Full changed 446,838 of 130,965,504 RGBA bytes.
The maximum difference was one.

Development now calibrates and packs the crop from strided source rows.
It no longer copies the float crop into another full output allocation.
The real-fixture test shows exact RGBA equality with the copied-crop reference.
This removes a 93 MiB temporary allocation for Browse and a 374 MiB temporary allocation for Full.

The disk develop version changed from 2 to 3.
This change prevents use of pixels produced by the old transfer path.

### JPEG persistence

RAW workers now publish developed pixels before JPEG persistence.
A single background worker performs JPEG encoding, disk writes, and optional RAM-JPEG insertion.
The producer uses a nonblocking enqueue operation.
Pending requests coalesce by image and tier.

The pending RGBA-reference budget is 256 MiB.
One active RGBA request and all pending RGBA requests can retain about 512 MiB in the persistence lane.
This value is not a total process-memory limit.
It excludes the encoded JPEG, the RAM caches, queue metadata, and other application data.
A request can become a best-effort cache miss when the lane is busy, full, closed, or too large.
There is no retry or metric for this event.
The pixels remain available in the RGBA RAM cache when a display request completed.
The system can develop the image again after a dropped cache request.
A dropped warm-only request discards its developed pixels and can repeat that work later.

Engine shutdown drains requests that the persistence lane accepted before it closed.
Develop workers are detached.
A develop worker that completes after the lane closes cannot enqueue persistence work.
This can delay application shutdown or any operation that replaces the engine.
The delay is bounded by the active request and the accepted pending work.

The earlier synthetic baseline measured 12.2 MP JPEG encoding at 47.94 ms for quality 80 and 110.17 ms for quality 92.
Moving this work removes that encode time from develop-worker occupancy.
This statement is a path analysis, not a measured end-to-end latency result.
It does not reduce total encode work.
The single persistence worker serializes encodes and can reduce persistence throughput.
It can still compete for CPU time because a thread yield does not establish a lower thread priority.

## Real RAW corpus

The private corpus contains four Sony ILCE-7CM2 ARW files.
All files use a 7,168 by 5,120 CFA source.
The repository ignores `testdata/`, so Git does not track these photos.

| File | Size | SHA-256 | Release CLI total |
|---|---:|---|---:|
| `HCA04875.ARW` | 36,237,312 bytes | `a649f862d6d5e9ec5b78bcc67cdd9860efe8ace776184f754b8703bcf78fa0f6` | 684.3 ms |
| `HCA05178.ARW` | 50,892,800 bytes | `69b695d8ecf7defbe936f1f5e1b37e01d8f864e4c5a4498cf6fe738d6bb3f9a8` | 753.1 ms |
| `HCA05354.ARW` | 40,796,160 bytes | `d2e5d891d6e3ba7c98aa2ac4fda1bc626b38df326e6ba3ccf054c7640f3840e0` | 563.5 ms |
| `HCA05417.ARW` | 40,038,400 bytes | `8281c506002b86530806ba1f7e0e1109b5f6660c81868731ba98c0f84cd62e8a` | 559.1 ms |

Each CLI total is one release-mode process sample.
It includes parse, metadata, entropy decode, Browse develop, Full develop, and two JPEG encodes.
Do not use these four samples as a regression gate.
Use the Criterion RAW group for repeated comparison.

`HCA05417.ARW` has a 270-degree orientation tag.
An opt-in test verifies that its 360-pixel thumbnail is display-oriented and portrait-shaped.
The corpus covers one camera model.
The real-RAW transfer and crop checks use one of these fixtures.
The dense synthetic checks cover the complete transfer-table input domain, but the real-camera result is empirical evidence for this fixture.

## Correctness evidence

The final test set has these checks:

- Exhaustive comparison of the optimized navigation planner with the reference planner.
- Stable priority and FIFO checks for a 512-job queue.
- Deterministic RAM-cache reference comparison and structural invariants.
- Bit-exact rotation checks.
- Empty-width and empty-height quarter-turn checks.
- Exact Bayer superpixel comparison for all four patterns.
- Dense transfer-table bounds and a real-RAW maximum-error check.
- Exact strided-crop comparison on Browse and Full real-RAW output.
- Persistence contention, coalescing, saturation, memory budget, cancellation, and shutdown checks.
- Atomic cache and XMP replacement checks.
- Durable rating flush and no-change fast-path checks.

The full workspace run passed 93 tests.
Three private-corpus tests are ignored by default.
All three ignored tests passed in a single-threaded release run.

## Environment

- Hardware: Apple M5 with 10 CPU cores and 24 GB memory.
- Operating system: macOS 26.2 on arm64.
- Rust: 1.96.1 with LLVM 22.1.2.
- Benchmark profile: `bench` with thin LTO.
- Criterion plots: Disabled.
- RAW fixture for repeated comparison: `HCA04875.ARW`.

The synthetic comparison used the `grind-before` baseline.
The isolated RAW comparison used the `raw-before` baseline.
The final RAW binary used a new Cargo target directory.
This prevented reuse of historical benchmark artifacts.

## Reproduce the checks

Run the quality checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo bench -p viewr-core --bench core_hot_paths --no-run --locked
cargo build --workspace --release --locked
```

Run the private-corpus tests:

```sh
cargo test -p viewr-core --lib --release --locked -- --ignored --test-threads=1
```

These tests expect `HCA04875.ARW` and `HCA05417.ARW` in `testdata/real-raw-corpus/`.

Run one repeated RAW benchmark:

```sh
VIEWR_BENCH_RAW=/absolute/path/photo.ARW \
  cargo bench -p viewr-core --bench core_hot_paths --locked -- raw_opt_in --noplot
```

## Remaining work

- The app still creates filmstrip widgets for the full visible sequence.
  Cache probes now scale with the viewport, but full widget virtualization needs a separate UI change.
- The disk-warm navigation plan still scales linearly with the number of warm targets.
- The thumbnail comparison needs a repeated isolated run before optimization work.
- End-to-end interaction latency needs an app-level benchmark with input-to-frame timing.
