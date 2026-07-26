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
| P1 | SQLite used lossy path text as the rating key. Two native paths could use one row. | Fixed. Valid UTF-8 paths keep compatible text keys. Other native paths use reversible BLOB keys, and mixed TEXT/BLOB or Windows component-equivalent keys fail closed instead of collapsing in memory. |
| P1 | A rating row did not verify the RAW size and modification time during folder load. | Fixed. A row applies only to the matching RAW identity. |
| P1 | Startup recovery could write an old rating beside a replaced RAW file. | Fixed. Recovery checks the current RAW identity before each write. A conditional delete cannot remove a newer journal row. |
| P1 | An old sidecar completion could clear a newer dirty rating. | Fixed. Completion uses a conditional compare operation on path, identity, and rating. |
| P1 | A delayed current-version Viewr process could replace XMP after another process completed a newer rating. | Fixed. A physical sidecar-owner revision and an immediate SQLite ownership transaction now cover the ownership check, durable XMP replacement, and database completion. |
| P1 | A transient initial journal failure could leave an older dirty rating authoritative. | Fixed. Unjournaled work remains pending and must establish database ownership before it can publish XMP. |
| P1 | Parent symlinks, filesystem-verified case and Unicode aliases, and ARW/DNG siblings could name one XMP target through different database paths. | Fixed for the supported identity probes. Current writes use a filesystem-derived physical owner. Legacy histories are read conservatively, ambiguous dirty work is quarantined, and a retargeted parent symlink cannot redirect recovery. Linux bind mounts and distinct case-folded mount spellings remain an operational limit. |
| P1 | A configured database directory or open failure could silently downgrade rating writes to database-free XMP publication. | Fixed. The configured database remains authoritative; work stays queued until it can journal safely. |
| P1 | A damaged or interrupted schema repair could reuse an optimistic retry token or expose stale ownership through the read-only startup path. | Fixed. Repair intent is durable, every retry domain crosses a barrier, malformed counters fail closed before arithmetic, and read-only startup rejects incomplete repair state. |
| P1 | Simultaneous first opens could combine schema observations from different migration states and reject an otherwise valid database. | Fixed. Readiness decisions use one SQLite snapshot, repair selection is serialized, stale forced-repair decisions accept a capability state completed by another opener, and final verification tolerates a cooperating opener's temporary repair sentinel. The process test runs four independent eight-writer start bursts, and damaged-schema openers have a separate concurrency test. |
| P1 | A removed clean legacy alias did not invalidate unordered dirty same-name history, and a partially migrated database handled ownerless/owned ambiguity asymmetrically. | Fixed. Clean unresolved paths poison only dirty recovery candidates, mixed migration carries ambiguity in both directions, and focused read/migration tests cover distinct legacy owner spellings. |
| P1 | An ownerless fallback could be displayed or promoted after a newer shared-owner/path ledger, even though the two states had no comparable per-owner position. | Fixed. Ownerless rows remain usable only when their exact representation family has no separate path history and their derived XMP owner has no ledger. A valid current owned row still outranks rejected ownerless history, and one failed startup owner probe cannot be reinterpreted later in the same snapshot. |
| P1 | Older Windows databases could retain ordinary drive or UNC paths, while current canonicalization adds the verbatim namespace prefix and could mistake the same physical path for an unsafe alias. | Fixed. Migration accepts only an exact ordinary-to-verbatim prefix change, retains the historical path/revision key, assigns a filesystem-derived current owner only to resolvable identity-valid rows, and continues to quarantine component changes, target-owner/path tombstones, or colliding legacy histories. Canonical, uppercase-drive, and lowercase-drive keys are checked as one raw-spelling family without using `Path` equality. Indexed startup lookup keeps an offline clean row readable after reconnect when no newer history exists. Windows regressions cover pre-migration reads, clean fallback, dirty recovery/publication, pre-v8 owned rows, drive-case path and owner tombstones, mixed ownerless/owned collisions, duplicate raw keys, drive-root-relative rejection, and malformed UTF-16 keys. |
| P1 | SQLite can store same-name indexes and triggers, allowing a valid object to mask a hostile opposite-type object. | Fixed. Readiness checks both namespaces and repair removes both before installing canonical objects. |
| P1 | A panic in one decode job left its ID in flight and removed one worker. | Fixed. Each claimed job now has a panic boundary. Cleanup occurs before failure publication, and the worker continues. |
| P1 | `ByteLru::remove` performed a required map removal inside `debug_assert_eq!`. | Fixed. Release builds now perform the removal before the assertion. The new release test gate found this defect. |
| P2 | A canceled decode generation could publish a stale panic failure. | Fixed. Only the current uncanceled generation can publish a panic event, and background warming remains invisible to the UI event channel. |
| P2 | Atomic sidecar replacement lost the prior file permissions and destroyed a symbolic link. | Fixed. Replacement keeps file permissions and rejects symbolic-link targets. |
| P2 | Non-finite configuration values could enter layout and budget calculations. | Fixed. Non-finite values now use documented defaults. |
| P2 | Invalid metadata could create zero-denominator exposure text or non-finite color math. | Fixed. Invalid rationals, matrices, and white-balance values now fail closed. |

The rating journal now uses stronger local and database checks.
Cooperating Viewr processes that use the same database serialize sidecar
ownership before touching XMP.

The latency-sensitive folder-open path can read the two supported legacy
rating schemas without migrating them. It selects conservative candidates,
then derives and verifies physical owners before applying a row. The background
persistence worker remains the only migration writer. The app keeps folder
open non-blocking and refreshes persisted ratings on a detached thread after
the worker has completed its initial recovery pass. This reconciliation also
runs for an initially readable current schema, because its first snapshot can
race recovery. Explicit user ratings and already-arrived embedded metadata
retain their precedence during that refresh.

Current public clean upserts still accept an unresolved path and preserve its
spelling. Dirty sidecar journaling now rejects an unresolved physical owner,
`Db::open` requires a WAL-capable storage location, and the public exhaustive
`DbError` enum adds `WalUnavailable`. Those are pre-1.0 behavioral and
source-level breaking changes. The new `Db::try_open_for_read` API is additive:
it returns `Result<Option<Db>, DbError>` so callers can distinguish
read-compatible state from a database that needs background migration. The
breaking commit and release configuration explicitly select release 0.2.0.

Configured persistence does not downgrade to database-free XMP when opening,
migrating, or enabling WAL fails. It retains in-process work and retries.
SQLite files on network storage or managed profiles that decline WAL are
therefore unsupported. On Windows, the current platform configuration path is
Roaming AppData; managed deployments must ensure it remains local and
WAL-capable.

Viewr 0.1.x predates the journal gate and can write XMP after a database write
fails. The v8 fence contains the exact current path when possible, but SQLite
cannot make alternate sibling or alias paths safe. Close every older process
that can write the same photo folders before the first 0.2.x launch, and do not
relaunch or downgrade to 0.1.x for folders or databases already used by 0.2.x.

## Performance changes and measured decisions

### Navigation rating flush

Navigation waited for durable XMP and SQLite work after a rating change.
The wait included file synchronization and parent-directory synchronization.

The application now sends an ordered asynchronous flush request.
The FIFO channel attempts the journal write before it handles the flush.
Shutdown blocks for a bounded sequence of best-effort flush attempts.
It reports any updates that remain unpersisted after those attempts.
Persistent journal failure can still discard an unjournaled update at exit.
A failed requested flush restores the dirty state while the process runs.

### Disk-cache GC

Engine destruction joined a complete cache scan and sort.
A folder switch could wait for that work on the application thread.

Cache maintenance now runs as detached, best-effort work.
Within one Viewr process, at most one scan runs for a cache root.
Separate Viewr processes can scan the same root at the same time.
Engine destruction does not join the cache-maintenance thread.

The new scan benchmark measured warm, under-budget passes.
Each pass scans object metadata but does not sort or delete objects.
The benchmark measured these local medians:

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
This benchmark uses a warm, in-memory database and an all-hit path set.
It measures point-query and lookup CPU costs.
It does not measure database-open or cold disk costs.

### Rating database lifecycle

Folder startup now resolves current-schema ratings and owners in bounded
queries instead of issuing one database query per entry. Current-schema
readiness checks table and object metadata without scanning image or owner
rows. Repair-only value validation remains linear because it must inspect
counter storage classes before performing arithmetic.

The expanded harness separates current reopen, legacy compatibility, repair
stress, pending scans, and cold migration:

| Workload | Local measured interval |
| --- | ---: |
| Current read-write reopen, through 50,000 rows | 0.44–0.62 ms |
| Current read-only reopen, through 50,000 rows | 0.32–0.38 ms |
| Selective legacy load with 50,000 history rows | 13.9–22.1 ms |
| Full-scan legacy reference with 50,000 history rows | 94–115 ms |
| Cold released-v0.1.0/v0.1.1-to-v8 migration, 1,000/10,000 rows | Covered; stable baseline not yet recorded |
| Cold released-v0.1.0/v0.1.1 all-online migration, 1,000 rows | Covered; stable baseline not yet recorded |
| Cold unresolved v7-to-v8 migration, 1,000 rows | 17.9–19.1 ms |
| Cold unresolved v7-to-v8 migration, 10,000 rows | 182.7–204.9 ms |
| Cold online-owned v7-to-v8 migration, 1,000 rows | Covered; stable baseline not yet recorded |

The current reopen results remained flat across the measured row counts.
Legacy dirty and repeated-stem histories are intentionally conservative and
can approach a full scan. At 10,000 history rows those adversarial shapes
measured approximately 102–138 ms, while zero-dirty histories measured
approximately 2.7–3.6 ms. These are local scale measurements, not release
thresholds.

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
- Warm, in-memory, all-hit rating database lookup for up to 50,000 entries.
- Complete current-schema folder rating hydration for up to 50,000 entries.
- Current read-write/read-only database reopen through 50,000 image and owner
  rows.
- Selective legacy startup paired with a full-scan correctness reference,
  including dense-dirty and repeated-stem adversarial histories.
- Cold migration from both released ownerless schemas: v0.1.0 without the
  journal column and v0.1.1 with sparse unfinished work. Both use existing and
  missing RAWs, persistent WAL mode, correctness preflights, and a fresh
  database copy for every timed iteration; paired all-online cases exercise
  successful clean owner assignment.
- Cold migration from the exact released v7 column shape, including the
  missing ownerless revision column, from a fresh database copy on every
  measured iteration, with separate unresolved-removal and online-owner-rekey
  corpora.
- Indexed pending-journal scans with zero and one dirty row, plus journal
  updates against owner ledgers through 50,000 rows.
- Production priority-queue synchronization.
- Warm, under-budget disk-cache GC scans for up to 10,000 objects.
- Browse and Full JPEG work at production dimensions and quality values.
- Shared-owner group construction and rating installation against prefilled
  maps through 100,000 entries. The install case executes a threshold-filter
  transition predicate; it isolates this primitive and is not end-to-end UI
  latency.

The suite removed a duplicate planner benchmark.
Navigation benchmarks now report latency instead of misleading folder-element throughput.
An absent `VIEWR_BENCH_RAW` value now produces an explicit skip message.

The manual benchmark workflow now stores `benchmark-metadata.json`.
The file records the commit, lockfile hash, toolchain, OS, CPU, memory, Rayon setting, and cache condition.
Criterion results and metadata remain available for 90 days.

The optimized CI job now runs the complete workspace test suite with the release profile.
This gate executes optimized-only parallel code and detects release-only assertion mistakes.
CI also reports that private RAW fixtures are absent instead of silently implying camera coverage.
The same job smoke-runs both Criterion harnesses instead of only compiling them.
The macOS and Windows test jobs compile every all-feature benchmark target;
Linux remains the single optimized runtime-smoke host.
Benchmark preflight assertions verify that queue and XMP workloads cannot
silently become no-ops. The one-iteration CI smoke also starts the rating
journal target in update state, the RAM cache at its eviction limit, the
texture LRU with asserted resident hits, and the filmstrip with a nonempty
bounded viewport.
The review rejected a hard source-coverage percentage gate until the project
has an advisory baseline and a policy for UI-heavy and private-RAW paths.

## Deferred work

### External sidecar writers

The ownership transaction serializes Viewr processes that share the same
SQLite database.
Other applications do not participate in that transaction.
A valid external sidecar rating wins when its modification time is at least the
clean database value.
An unfinished dirty journal stays authoritative.
Startup recovery can replace an external change that occurred while that
journal was dirty.

### Unresolved-path retry scope

An unresolved RAW has no safe physical sidecar owner. Its optimistic retry
therefore watches a global ownerless epoch. An unrelated ownerless clean update
can conservatively discard that retry. This favors preventing a stale XMP
publication over retaining a rare update made while the RAW or its containing
filesystem is unavailable.

### Database trust boundary

Warm current-schema opens validate capability metadata in constant time.
Migration and repair paths additionally scan counter storage classes and
domains before arithmetic. Arbitrary row tampering performed outside Viewr
while every schema object remains canonical is treated as external database
corruption; continuously scanning every ledger row on normal startup was
rejected because it would make warm open time proportional to library history.

### Filesystem identity limits

Physical-owner resolution covers ordinary parent symlinks and case or Unicode
spellings that the filesystem probe can verify. Linux bind mounts and unusual
case-folded mount aliases can still canonicalize to different paths for the
same file. Viewr cannot safely unify those spellings without a durable,
cross-platform file-identity layer. Use one mount spelling for a photo folder.

Windows migration treats `C:\...` and `\\?\C:\...` (and the corresponding UNC
forms) as the same historical spelling only when every raw UTF-16 code unit
after the namespace prefix is identical. Drive-root-relative paths, changed
case or components, dot/separator rewrites, device namespaces, junctions, and
symlinks remain unsafe. Valid resolvable rows keep their original path and
revision ledger as journal provenance while receiving the verified canonical
sidecar-owner key. Offline clean rows remain ownerless; indexed exact-prefix
lookup makes them visible after reconnect, and a later write can assign the
verified owner. Any target-owner ledger or separate member of the exact
canonical/ordinary path-spelling family blocks ownerless fallback or
publication so older work cannot overtake newer history.

An owned dirty row with a `NULL` rating is a deliberate unrating tombstone
created when the compatibility fence contains an obsolete writer. It is not a
publishable pending sidecar, but it remains authoritative through forced
schema repair so an obsolete XMP value cannot reappear. An ownerless dirty
`NULL` has no durable publication identity and is quarantined during repair.

When a pre-v8 parent alias is ambiguous, migration quarantines unfinished
same-name histories across directories because the old schema has no
historical owner or total order. This can discard an unrelated legacy DB
fallback with the same normalized stem. The XMP file remains readable; the
availability cost is deliberate to prevent an unordered legacy rating from
being published.

### Persistence storage, contention, and growth

The configured database must support SQLite WAL and local locking. A location
that declines WAL leaves updates queued and never falls back to unjournaled
XMP. Windows currently uses Roaming AppData through the platform configuration
API, so a network-backed managed profile is an operational risk.

A durable XMP replacement occurs while an immediate SQLite transaction owns
the rating. Slow external storage can therefore delay unrelated rating writers;
the benchmark suite does not yet include multi-process or slow-filesystem
contention. Revision ledgers and the quarantine table also have no automatic
compaction, and the in-memory command backlog is not bounded while a configured
database remains unavailable.

### Startup refresh integration coverage

Core tests verify that the readiness callback follows initial recovery, and app
unit tests verify refresh backoff and rating-source precedence. There is not yet
an end-to-end headless test that drives the egui session, background migration,
folder replacement, and repaint loop together.

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
- Normalize a selected file to the same physical spelling returned by folder
  scanning.
- Read rating state without migrating on the application thread, then
  reconcile it after background startup recovery.
- Keep rating state consistent for ARW/DNG or filesystem aliases that share one
  physical XMP owner.

No widget construction, paint path, layout behavior, style value, control, or visible label changed.
The loupe, filmstrip, settings, color, and texture-LRU source files are unchanged.
The shared-owner correction can update multiple existing star displays and
their filter membership after one rating because those entries persist to the
same XMP file. This is a data-consistency correction, not a presentation or
interaction-design change. The invariant is enforced by source-boundary review
and behavior tests; the project does not yet have screenshot-golden coverage.

## Verification commands

The final verification set is:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo test --workspace --release --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo build --workspace --release --locked --bins --benches --all-features
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- --test
cargo bench -p viewr --features benchmarks --bench filmstrip_scaling --locked -- --test
cargo +nightly-2026-07-21 miri test -p viewr-core --lib resize::tests::rotate_ --locked
cargo +nightly-2026-07-21 miri test -p viewr-core --lib \
  develop::tests::superpixel_output_initialization_is_valid_under_miri --locked
```

The ordinary and release test output reports the three absent private-RAW fixtures as ignored.

Final verification passed:

- The debug and release workspace suites each reported 312 passed unit tests
  and three ignored private-RAW tests.
- The app reported 39 passed unit tests.
- The core library reported 273 passed unit tests and three ignored private-RAW tests.
- The Rustdoc suite reported one passed documentation test.
- Clippy reported no warnings across all targets and features.
- The all-feature release bins and benchmark targets built successfully.
- Both optimized Criterion harnesses completed their runtime smoke checks.
- Miri reported four passed unsafe-path tests.
- Seventy repeated cross-process parent tests completed 2,240 synchronized
  child opens and writes without a failure.
- Fifty repeated eight-thread damaged-schema tests each converged on one
  generation barrier and one owner-repair barrier.
- The base-to-branch whitespace check passed.
- The rendering, layout, settings, color, and texture-LRU files had no source changes.
