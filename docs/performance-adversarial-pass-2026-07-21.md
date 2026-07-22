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
| Loupe filmstrip, 10,000 placeholder items | 9.632 ms | 15.349 µs | about 628 times faster |
| Loupe filmstrip, 50,000 placeholder items | 52.138 ms | 14.214 µs | about 3,668 times faster |
| Thumbnail texture LRU, touch 200 of 773 residents | not previously bounded | 37.678 µs | bounded maintenance cost |
| Bounded planner with disk configured, 10,000 items | 9.426 µs folder-wide reference | 78.627 ns | about 120 times faster |
| Per-file background RAW work | 6.885 ms | 762 µs | about 9.0 times faster |
| Canonical XMP rating update | 35.293 µs | 15.747 µs | about 2.24 times faster |

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
and shutdown have focused queue tests. The 120-times comparison is a pure
planner measurement; it excludes Engine locks, cache probes, queue
synchronization, and the one-time O(N) warm-order installation.

Persistence backpressure parks size-aware warm jobs until capacity changes.
Active or pending encodes absorb matching warm obligations, and a canceled job
that already handed off pixels is not developed again. A mutex-busy enqueue has
one short blocking fallback; saturation admits one fitting retry per completed
persistence request. Oversized buffers and permanent writes after three bounded
attempts remain explicitly best-effort.

Folder-wide light work now extracts metadata only. Embedded preview JPEG decode
and RGBA allocation happen on viewport demand. On `HCA04875.ARW`, the warm
Criterion point estimate changed from 6.885 ms for thumbnail plus metadata to
762 µs for metadata alone. Embedded rating and filter semantics remain intact.

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

The named baseline assumed a literal `xmp` prefix and returned immediately from
an early attribute. URI-aware RDF scoping and full-tail validation necessarily
do more parsing. Final attribute reads measured 15.664 µs and late element
reads measured 15.210 µs. The early-attribute case is about 64.5 times slower
than its shortcut baseline; the late-element case is about 42% slower. These
in-memory measurements exclude file I/O and remain small beside RAW work.

The canonical write path now validates the complete XML document but replaces
only the borrowed attribute-value byte range instead of owning and rewriting
every event. Its focused estimate was 15.747 µs: 56% faster than the named
baseline and about 71% faster than the initial hardened implementation. The
slower reads are an explicit correctness tradeoff; the write optimization
was promoted only after namespace-shadowing, escaped-URI, duplicate-attribute,
reserved-binding, RDF-context, unrelated-element, formatting-preservation, and
truncated-document tests passed.

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
  prefixes, and require an RDF `Description` context. Rating elements count
  only as direct RDF properties. Empty, self-closing, text, and CDATA forms
  update without creating duplicate attributes. Malformed tags, duplicate
  attributes, invalid namespace bindings, and truncated tails fail closed.
  Existing XML with no RDF rating subject now returns an error instead of
  reporting success without changing the rating; file-level tests verify that
  the original sidecar remains byte-for-byte intact on this failure.
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
- Notification callback panics are caught at the worker boundary and logged,
  so a UI repaint callback cannot strand an in-flight job or terminate a decode
  worker.

## Rustdoc and architecture documentation

Both crates now deny missing public documentation and broken intra-doc links.
The core crate landing page describes the scheduling, cache, and rating
persistence architecture and includes a compiled navigation-planning example.
Public APIs document error and panic conditions, pixel-storage invariants,
cache budget and pin behavior, cancellation boundaries, worker ownership,
rating precedence, sidecar recovery, and the byte-exact versus semantic XMP
update paths.

The README now matches the demand-driven thumbnail path, Fit-mode Full-work
elimination, persistent Browse warming, logical GPU accounting, soft RAM and
disk targets, native SQLite dependency, and dirty-journal precedence. The
benchmark guide distinguishes pure planner proxies, operating-system page
cache from viewr caches, and the complete RAW decode path. Historical milestone
notes remain intact but point to this report as the current architecture.

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

## Final validation

The final committed source tree passed these gates:

- `cargo fmt --all -- --check` and `git diff --check`;
- strict workspace Clippy across all targets with warnings denied;
- 31 app unit tests, 131 core unit tests, and one compiled Rustdoc example;
- all three ignored, private-RAW release tests when explicitly enabled;
- workspace Rustdoc with warnings denied;
- workspace release build and benchmark-target compilation; and
- Miri's three rotation tests plus the direct superpixel initialization oracle
  on pinned nightly `2026-07-21`.

Three private-fixture tests remain ignored in an ordinary workspace run by
design. Their four Sony ARW source files remain under ignored `testdata/`; Git
tracks none of the photos.

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
- The persistent warm order and its queue memory are O(N) once at folder open.
  Continuous interactive work can intentionally starve warming. Oversized
  buffers and disk writes that still fail after three attempts remain
  best-effort cache misses rather than unbounded retry loops.
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
- Notification callback panics are contained. A panic elsewhere inside a
  decode worker can still terminate that worker and leave its job generation
  recorded as in flight. An unwind-safe finish guard would improve recovery,
  but cannot repair a dependency panic that poisons shared state.
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
- XMP token scanning is not a full DTD/general-entity or single-root validator,
  and UTF-16 sidecars are unsupported. Multiple semantic ratings use
  first-read/all-updated behavior; element or injection fallbacks can
  reserialize XML, while existing attribute updates are byte-spliced.
- The private corpus contains one camera model. Add more codecs, sensor sizes,
  orientations, and DNG samples before treating RAW numbers as universal.

## Reproduce focused checks

```sh
cargo bench -p viewr --features benchmarks --bench filmstrip_scaling --locked -- --noplot
cargo +nightly-2026-07-21 miri test -p viewr-core --lib resize::tests::rotate_ --locked
cargo +nightly-2026-07-21 miri test -p viewr-core --lib \
  develop::tests::superpixel_output_initialization_is_valid_under_miri --locked
```

Use the full quality and RAW commands in
[`testing-and-benchmarking.md`](testing-and-benchmarking.md).
