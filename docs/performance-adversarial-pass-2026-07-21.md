# Performance and adversarial review — second pass — 2026-07-21

This pass followed the initial optimization campaign with another named baseline,
native sampling, adversarial concurrency and architecture reviews, Miri checks,
and app-level UI scaling benchmarks. Private RAW files stayed under ignored
`testdata/`; no photo is tracked by Git.

## Rust guidance used

The official curated skill catalog did not contain a Rust-specific package.
Five script-free public skills were inspected, pinned to exact source commits,
and installed locally:

- `rust-patterns`, `rust-testing`, and `benchmark-optimization-loop` from
  `affaan-m/everything-claude-code` at
  `5deee34c93395045b985e3baf91550e5f1ab7204`.
- `rust-profiling` and `rust-sanitizers-miri` from
  `mohitmishra786/low-level-dev-skills` at
  `bdc58472fa9f309ed1b3f7d985a0d8e9bd8f4608`.

Candidate skills with unsafe, stale, or misleading blanket advice were not
installed. The selected guidance shaped the benchmark promotion gate, native
sampling, property tests, and narrowed Miri coverage. Installed skills become
available automatically in new Codex tasks.

## Measured findings

Lower time is better. Measurements were taken on the same Apple M5 computer as
the first campaign. Criterion results use release-like benchmark builds.

| Workload | Before | After | Result |
|---|---:|---:|---:|
| Loupe filmstrip, 10,000 placeholder items | 7.221 ms | 14.905 µs | about 484 times faster |
| Loupe filmstrip, 50,000 placeholder items | 47.817 ms | 15.693 µs | about 3,047 times faster |
| Thumbnail texture LRU, touch 200 of 773 residents | not previously bounded | 37.247 µs | bounded maintenance cost |
| 10,000-item disk-warm navigation plan | 11.863 µs | 83.919 ns | about 141 times faster |
| Per-file background RAW work | 8.293 ms | 778 µs | about 10.7 times faster |
| Canonical XMP rating update | 35.293 µs | 15.874 µs | about 2.22 times faster |

The filmstrip benchmark is headless and excludes texture upload, tessellation,
and GPU work. It isolates widget construction. Before virtualization, the lower
bound alone used most of a 16.7 ms frame at 10,000 images. The new work is tied
to viewport width plus two overscan columns, not folder length.

The thumbnail texture map now has a 256 MiB logical RGBA budget and uploads at
most eight textures per frame. A typical 360 by 241 RGBA corpus thumbnail is
347,040 bytes. The old unbounded map could therefore retain at least 3.23 GiB
for 10,000 thumbnails or 32.3 GiB for 100,000 thumbnails, before backend
overhead.

Disk warming now has a persistent, strictly lower-priority queue. Navigation
rebuilds only the bounded interactive plan; it no longer reconstructs and
heapifies a folder-wide warm set. The one-time outward order remains linear at
folder open. Foreground replacement, canceled-generation retry, filter changes,
and shutdown have focused queue tests.

Folder-wide light work now extracts metadata only. Embedded preview JPEG decode
and RGBA allocation happen on viewport demand. On `HCA04875.ARW`, the warm
Criterion point estimate changed from 8.293 ms for thumbnail plus metadata to
778 µs for metadata alone. Embedded rating and filter semantics remain intact.

### RAW profile

A macOS `sample` profile captured a direct Criterion Full develop run. The run
estimated Full development at about 149 ms. Most measured develop time was in
rawler's parallel PPG demosaic; the viewr calibration and packing passes were a
much smaller share. This supports avoiding unnecessary Full work instead of
adding risky micro-optimizations around the dominant third-party algorithm.

Fit mode now schedules, pins, and uploads no Full render. Zoom mode keeps the
current and near Full window. On the 7,168 by 5,120 fixture, each avoided Full
RGBA buffer is about 131 MB, before its JPEG encode and GPU upload. This is a
work-elimination result; the pure develop benchmark itself is unchanged.

### Cold thumbnail path

Fresh-process corpus probes measured a median first thumbnail pass of about
27.2 ms and an immediate-repeat median of about 9.0 ms. One selected Sony ARW
measured 61.6 to 63.5 ms on the first pass and 6.7 to 8.2 ms on repeat. The
existing Criterion thumbnail result measures the warm path and does not expose
this startup gap.

Inspection found that rawler requests full-mapping sequential read-ahead on
Unix; Linux and Android also use populated mappings. A preview-specific rawler
source API is the likely fix. Disabling read-ahead globally was not attempted
because it can make later RAW entropy decode slower.

### XMP tradeoff

The named baseline assumed a literal `xmp` prefix. URI-aware aliases and
fail-closed tag validation necessarily do more parsing. Final attribute reads
measured about 877 ns, 174% above that baseline, and a late element read
measured 15.305 µs, 40% above baseline. Both remain small beside RAW work.

The canonical write path now validates the complete XML document but replaces
only the borrowed attribute-value byte range instead of owning and rewriting
every event. Its stable focused estimate was 15.874 µs: 56% faster than the
named baseline and about 70% faster than the initial hardened implementation.
The slower reads are an explicit correctness tradeoff; the write optimization
was promoted only after namespace-shadowing, escaped-URI, duplicate-attribute,
reserved-binding, and truncated-document tests passed.

## Correctness and resilience changes

- Job deduplication now includes the action, so a far `WarmDevelop` cannot
  suppress a foreground `Develop` for the same image and tier.
- Queue shutdown uses a mutex-protected condition predicate. Engine owns and
  joins decode, persistence, and GC workers instead of detaching them.
- Canceled generations cannot publish stale develop failures.
- Invalid RAM or disk JPEGs are evicted and fall back to RAW development.
- Disk GC passes are serialized per cache root. Recent atomic-write temporary
  files survive GC; only stale orphans are swept.
- Durable sidecar replacement synchronizes the parent directory after rename
  on Unix.
- SQLite records a dirty sidecar journal with each rating. Dirty DB ratings win
  after a debounce-window crash, failed writes remain retryable, and startup
  resumes unfinished sidecars.
- XMP rating reads and writes resolve the Adobe namespace URI, including alias
  prefixes. Empty, self-closing, text, and CDATA forms update without creating
  duplicate semantic ratings. Malformed or duplicate attributes fail closed.
- Disk cache keys validate their digest input and hash native path bytes, so
  invalid UTF-8 Unix names do not collide through lossy conversion.
- Folder entries are shared with the engine through one immutable `Arc`, so
  opening a large folder no longer deep-clones every path and display name.
- Persisted ratings load before decode workers start. Embedded metadata can no
  longer win or lose at startup according to worker timing.
- Cache pins follow filtered-visible neighbors from the same sequence snapshot
  as the navigation plan, rather than unrelated raw-index neighbors.
- Explicit zero ratings remain authoritative in memory, so delayed embedded
  metadata cannot resurrect a rating that the user just cleared.
- If a RAW has readable metadata but no usable embedded preview, a failed
  viewport thumbnail retains its failure event and performs one metadata-only
  fallback. Successful thumbnails never duplicate that metadata work.

## Unsafe-code checks

Nightly Miri passed all three repository-owned rotation tests and a direct
superpixel output-initialization oracle. The superpixel path uses a Miri-only
serial traversal because Miri cannot execute Rayon's Crossbeam deque pointer
scheme. Release builds keep the parallel traversal. A pinned nightly CI job
runs these narrowed checks.

A broad resize Miri filter stopped in a third-party ARM NEON intrinsic that
Miri does not implement. The original parallel superpixel reference test also
stopped inside Crossbeam under Miri's experimental borrow model. These are tool
limitations, not passing evidence, and are not reported as repository defects.

## Rejected optimization

A profile-led experiment fused color calibration and RGBA packing to remove one
frame traversal. It preserved pixels in a focused equivalence test, but paired
real-RAW Criterion comparisons detected no improvement:

- Browse change interval: -4.6% to +29.7%, with a +10.3% point estimate.
- Full change interval: -8.5% to +16.3%, with a +3.0% point estimate.

The experiment was removed. It did not meet the promotion rule and added
complexity around image color math.

## Remaining risks and next targets

- Metadata discovery still scans the folder once. In rawler 0.7.2,
  `RawSource::new` populates the whole file mapping and requests sequential
  read-ahead, so a very large cold folder can still stream substantial data
  even though preview JPEG decode is now viewport-driven. A preview/metadata
  source API without global read-ahead likely belongs upstream.
- Cancellation cannot interrupt rawler while a demosaic call is active. Engine
  destruction now joins workers, so a folder switch can wait for active RAW
  work to reach the next boundary.
- Full development remains dominated by rawler PPG demosaic. Further CPU work
  should begin upstream or with a deliberately different full-quality
  algorithm and image-quality corpus.
- Visible corrupt thumbnails retry with a fixed delay. Add capped exponential
  backoff or a terminal state for permanently unreadable files.
- Thumbnail `ByteLru::touch` is linear in resident textures. The measured
  200-touch/773-resident case is only 37 µs, but a materially larger GPU budget
  should prompt an intrusive or generation-based LRU.
- A panic inside a decode worker terminates that worker and can leave its job
  generation recorded as in flight. Panic containment or an unwind-safe finish
  guard would improve recovery, but cannot repair a dependency panic that
  poisons shared state.
- SQLite still identifies images with lossy path strings, while disk cache keys
  now use native path units. Non-UTF-8 Unix libraries can therefore still have
  database-key collisions even though render-cache keys do not.
- Render content identity uses path, size, and modification time. Replacing a
  file with different same-size bytes while preserving its mtime can reuse a
  stale cache entry; content hashing would close that uncommon case at an I/O
  cost.
- End-to-end input-to-present latency is not automated. The UI benchmarks
  isolate widget construction and texture-residency bookkeeping rather than a
  complete GPU frame.
- The private corpus contains one camera model. Add more codecs, sensor sizes,
  orientations, and DNG samples before treating RAW numbers as universal.

## Reproduce focused checks

```sh
cargo bench -p viewr --bench filmstrip_scaling --locked -- --noplot
cargo +nightly-2026-07-21 miri test -p viewr-core --lib resize::tests::rotate_ --locked
cargo +nightly-2026-07-21 miri test -p viewr-core --lib \
  develop::tests::superpixel_output_initialization_is_valid_under_miri --locked
```

Use the full quality and RAW commands in
[`testing-and-benchmarking.md`](testing-and-benchmarking.md).
