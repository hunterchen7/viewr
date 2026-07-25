# Deep performance, correctness, and architecture audit

Date: 2026-07-25

Audited base: `7b6f39c`

Audit branch: `codex/deep-performance-correctness-audit`

## Scope and invariant

This audit covers the complete Rust workspace and its GitHub Actions workflows.
It includes the application, the core library, tests, benchmarks, persistence, caches, and documentation.

The audit used one strict product invariant: the user interface must not change.
No paint code, layout code, style value, control, label, or interaction design changed.
Backend timing can change when data becomes ready.

Three independent audits examined these areas:

- Correctness, persistence, filesystem safety, and concurrency.
- Runtime performance, memory limits, scheduling, and cache architecture.
- Tests, Criterion coverage, CI gates, fixture coverage, and benchmark reproducibility.

## Baseline

The base workspace passed all normal tests.
The base result was 163 passed tests and three ignored private-RAW tests.
The strict all-target Clippy run also passed.

The audit host had this configuration:

- Apple M5 with 10 logical cores.
- 24 GB memory.
- macOS 26.2 on ARM64.
- Rust 1.96.1.
- LLVM 22.1.2.

The first full Criterion run overlapped another benchmark process.
The audit does not use that run as comparison evidence.
All promoted performance decisions use later isolated target runs.

## Correctness changes

| Priority | Finding | Decision and result |
| --- | --- | --- |
| P1 | Disk-cache GC followed arbitrary shard paths and treated unrelated files as cache objects. | Fixed. GC now accepts only real hexadecimal shards, valid cache names, and Viewr temporary files. Symlink and external-sentinel tests pass. |
| P1 | SQLite used lossy path text as the rating key. Two native paths could use one row. | Fixed. Valid UTF-8 paths keep compatible text keys. Other native paths use reversible BLOB keys. |
| P1 | A rating row did not verify the RAW size and modification time during folder load. | Fixed. A row applies only to the matching RAW identity. |
| P1 | Startup recovery could write an old rating beside a replaced RAW file. | Fixed. Recovery checks the current RAW identity before each write. A conditional delete cannot remove a newer journal row. |
| P1 | An old sidecar completion could clear a newer dirty rating. | Fixed. Completion uses a conditional compare operation on path, identity, and rating. |
| P1 | A panic in one decode job left its ID in flight and removed one worker. | Fixed. Each claimed job now has a panic boundary. Cleanup occurs before failure publication, and the worker continues. |
| P1 | `ByteLru::remove` performed a required map removal inside `debug_assert_eq!`. | Fixed. Release builds now perform the removal before the assertion. The new release test gate found this defect. |
| P2 | Atomic sidecar replacement lost the prior file permissions and destroyed a symbolic link. | Fixed. Replacement keeps file permissions and rejects symbolic-link targets. |
| P2 | Non-finite configuration values could enter layout and budget calculations. | Fixed. Non-finite values now use documented defaults. |
| P2 | Invalid metadata could create zero-denominator exposure text or non-finite color math. | Fixed. Invalid rationals, matrices, and white-balance values now fail closed. |

The rating journal now uses stronger local and database checks.
One cross-process sidecar race remains and is in the deferred section.

## Performance changes and measured decisions

### Navigation rating flush

Navigation waited for durable XMP and SQLite work after a rating change.
The wait included file synchronization and parent-directory synchronization.

The application now sends an ordered asynchronous flush request.
The FIFO channel journals the rating before it handles the flush.
Shutdown remains the blocking durability boundary.
Failure restores the dirty state for a later retry.

### Disk-cache GC

Engine destruction joined a complete cache scan and sort.
A folder switch could wait for that work on the application thread.

Cache maintenance now runs as detached, best-effort work.
Only one scan for a cache root runs at one time.
Engine destruction does not join the cache-maintenance thread.

The new scan benchmark measured these local medians:

| Cache objects | Scan time |
| ---: | ---: |
| 1,000 | 5.02 ms |
| 10,000 | 21.95 ms |

These values explain why the join did not belong in session destruction.
They are local reference values, not portable limits.

### RAM cache budget

The `ram_gb` setting claimed to set the total RAM cache budget.
The old allocation added a fixed 384 MiB thumbnail ring above that value.

The thumbnail ring is now part of the configured total.
The remaining bytes use the existing two-to-one RGBA and JPEG split.
The component budgets sum exactly to the configured value.

### Metadata queue candidate

The audit tested one heapify-once candidate for the folder metadata queue.
The existing insertion order already gives near-linear heap behavior.

| Entries | Existing median | Candidate median | Decision |
| ---: | ---: | ---: | --- |
| 1,000 | 47.20 us | 56.63 us | Reject |
| 10,000 | 404.43 us | 592.36 us | Reject |
| 100,000 | 4.51 ms | 4.20 ms | No reliable change |

The candidate regressed the 10,000-entry case by approximately 46 percent.
The implementation was reverted.
The production benchmark remains.

### SQLite batch candidate

The audit tested batched `IN` queries as a replacement for cached point queries.
Path cloning, query creation, and result hashing made the batch version slower.

| Rows | Existing median | Batch median | Change | Decision |
| ---: | ---: | ---: | ---: | --- |
| 1,000 | 478.07 us | 516.59 us | +7.9% | Reject |
| 10,000 | 4.62 ms | 5.84 ms | +27.9% | Reject |
| 50,000 | 24.45 ms | 30.27 ms | +26.1% | Reject |

The batch implementation was reverted.
The rating-lookup benchmark remains.

### Production reference measurements

The new queue benchmark includes the real priority heap and queued-ID index.
The 10,000-entry local median was approximately 0.86 us per navigation sync.

The JPEG benchmarks now use representative production sizes and exact production quality values:

| Work | Local median |
| --- | ---: |
| Browse 8 MP JPEG encode at quality 87 | 44.47 ms |
| Full 33 MP JPEG encode at quality 90 | 251.83 ms |
| Browse 8 MP JPEG decode | 18.10 ms |
| Full 33 MP JPEG decode | 104.51 ms |

These measurements have wide intervals on this host.
Use them as scale evidence, not as regression limits.

## Benchmark and CI architecture

The benchmark suite now includes these production-adjacent costs:

- Metadata queue construction for up to 100,000 entries.
- Rating database lookup for up to 50,000 entries.
- Production priority-queue synchronization.
- Disk-cache GC scans for up to 10,000 objects.
- Browse and Full JPEG work at production dimensions and quality values.

The suite removed a duplicate planner benchmark.
Navigation benchmarks now report latency instead of misleading folder-element throughput.
An absent `VIEWR_BENCH_RAW` value now produces an explicit skip message.

The manual benchmark workflow now stores `benchmark-metadata.json`.
The file records the commit, lockfile hash, toolchain, OS, CPU, memory, Rayon setting, and cache condition.
Criterion results and metadata remain available for 90 days.

CI now runs the core library tests with the release profile.
This gate executes optimized-only parallel code and detects release-only assertion mistakes.
CI also reports that private RAW fixtures are absent instead of silently implying camera coverage.

## Deferred work

### Cross-process sidecar serialization

The database compare operation prevents an old completion from clearing a newer rating.
It does not fully serialize XMP replacement across two Viewr processes.

A complete fix needs one ownership transaction or one operating-system file lock.
The lock must cover the ownership check, XMP replacement, and database completion.
This change needs a two-process fault-injection test before promotion.

### RAW pipeline concurrency

Browse and Full misses can decode the same RAW file independently.
Three Full workers can also retain multiple large mosaics and RGB frames outside cache accounting.

Do not change this policy without private RAW fixtures.
Measure current-image latency, cancellation, peak RSS, and output hashes first.
Use 24 MP, 33 MP, and 61 MP files.

### Worker topology

The engine uses three heavy workers, two light workers, and global Rayon threads.
This fixed topology can over-subscribe small CPUs and compete with JPEG persistence.

Test CPU counts of 2, 4, 8, and 10 before a policy change.
Record current-image latency and background throughput.

### Event and repaint pressure

Metadata work sends one event and one repaint request per file.
The application drains the complete event queue in one frame.

Add queue-depth and frame-time counters before event coalescing.
The test must preserve filter and metadata update order.

### Texture and transient memory

Main image textures do not use one explicit byte budget.
GPU upload also creates one RGBA copy.

Add backend memory telemetry before a texture-residency change.
An eviction change can alter placeholder timing.

### Filesystem hardening limits

GC ignores symbolic links that it observes.
A hostile process can still replace a checked directory before a later path operation.

A complete adversarial fix needs directory-handle operations such as `openat` and `unlinkat`.
Sidecar replacement also does not preserve ACLs or extended attributes.

### Startup and module boundaries

`jobs.rs` still combines queue policy, persistence, worker execution, cache recovery, and tests.
`app.rs` still combines session control, input, textures, and rendering.

Split these modules only after the orchestration benchmarks cover the new boundaries.
First isolate scheduler, persistence, and worker execution behind the current `Engine` API.

### Private RAW coverage

Three real-camera tests remain ignored because the repository has no licensed RAW corpus.
Synthetic tests do not replace camera, codec, orientation, and metadata coverage.

Keep private files untracked.
Record fixture hashes, camera models, dimensions, and codecs for every private run.

## User-interface invariant

The application changes are limited to these nonvisual operations:

- Replace a blocking persistence call with an ordered asynchronous request.
- Divide the configured RAM budget across the three backend cache rings.
- Reject invalid numeric configuration values.

No rendering function, widget tree, layout value, style value, control, or visible label changed.
The loupe, filmstrip, settings, color, and texture-LRU source files are unchanged.

## Verification commands

The final verification set is:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo test -p viewr-core --release --lib --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo +nightly-2026-07-21 miri test -p viewr-core --lib resize::tests::rotate_ --locked
cargo +nightly-2026-07-21 miri test -p viewr-core --lib \
  develop::tests::superpixel_output_initialization_is_valid_under_miri --locked
```

The ordinary and release test output reports the three absent private-RAW fixtures as ignored.

Final verification passed:

- The workspace reported 177 passed tests and three ignored private-RAW tests.
- The release core suite reported 143 passed tests and three ignored private-RAW tests.
- The Rustdoc suite reported one passed documentation test.
- Clippy reported no warnings across all targets and features.
- Miri reported four passed unsafe-path tests.
- The base-to-branch whitespace check passed.
- The rendering, layout, settings, color, and texture-LRU files had no source changes.
