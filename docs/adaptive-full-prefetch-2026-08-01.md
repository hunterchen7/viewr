# Adaptive Full-prefetch experiment — 2026-08-01

## Objective

The released Full working set held only the current image and the two adjacent
images. This fixed window left most of a large RAM budget unused.

The selected policy grows a Full-resolution working set toward its byte budget.
It gives priority to the current navigation direction. It also keeps foreground
response, memory accounting, filtered order, and the existing visual design.

## Memory policy

The `ram_gb` setting is a decimal-byte limit. The thumbnail ring receives up to
384 MiB. The current experimental policy divides the remaining developed-image
capacity as follows:

| Ring | Share of capacity after thumbnails |
|---|---:|
| Full RGBA | 60% |
| Browse RGBA | 20% |
| Encoded JPEG | 20% |

This split makes Full RGBA the largest ring, as required for aggressive
prefetch. It is a policy choice, not a measured optimum. The local tests do not
include a representative RAW corpus for end-to-end hit-rate, resident-memory,
foreground-latency, or throughput measurements. Future trace measurements can
change the split without changing the planner or cache architecture.

The cache records actual payload bytes for each Browse and Full render. A
Browse observation supplies a conservative five-times estimate until an exact
Full observation replaces it. Unknown images initially reserve 64 MiB for
Browse and 256 MiB for Full. The largest folder observation becomes the
fallback for other unknown images.

## Planning and eviction

The planner always selects the current Full render and its immediate visible
neighbors in filtered display order. These mandatory renders can exceed the
Full budget. This overrun can be material when the user selects a small RAM
limit and opens very large images.

Optional Full targets form one priority prefix with an approximately 3:1 bias
toward the current navigation direction. The planner stops at the first target
that would exceed the Full budget. It does not skip a large target to select a
less important target after it. The prefix can grow past one adjacent image and
can use the complete Full ring.

Browse targets use the same byte-aware prefix rule inside the existing bounded
navigation window. This prevents a fixed Browse wave from being larger than
the 20% Browse ring. It also gives the scheduler a resident fixed point instead
of repeatedly developing and evicting the same wave.

Each navigation transaction atomically installs these items:

- the normalized filtered order and current position;
- the current Browse and Full byte estimates;
- the new Full admission set and navigation pins;
- the replacement worker plan.

The Full ring immediately removes optional pixels that leave the admission
set. A worker cannot admit a late Full result after the item leaves that set.
Large pixel owners are released after the cache mutex unlocks, so allocator
work does not block other cache operations.

## Scheduling and persistence

The heavy dispatcher admits at most one speculative Full job. It can also admit
one folder-wide Browse warm job. With a fixed one-thread or two-thread
processing limit, these two background lanes run serially. Automatic mode and
fixed limits of three or more can admit both lanes.

Foreground jobs have higher queue priority. A navigation change cancels active
background generations at cooperative checkpoints. This is not a strict CPU
reservation. Nested Rayon work can occupy the complete processing pool, and a
non-interruptible decoder section can delay new foreground work.

An optional Full RAW miss stores developed pixels in RAM only. It does not
encode a JPEG or write the disk cache. If that image later becomes required,
the scheduler persists the resident pixels without developing the RAW again.
A disk object that was observed as present is not probed on every replan. A
later rehydrate miss clears that observation so the object can be rebuilt after
cache cleanup.

Promotion from speculative Full work to required display work reuses the live
generation. A replacement generation also checks RAM again before it decodes.
Therefore, a late result that reached RAM can be reused after an A-to-B-to-A
navigation sequence, and the same image is not decoded concurrently by two
generations. This is best-effort reuse. If the first generation observes
cancellation before cache admission, a later generation can decode A again.

Failed speculative Full targets remain suppressed for the current navigation
wave. Required display demand retries immediately. Optional errors do not
replace the visible image status.

## Display behavior

This change does not alter layout, colors, borders, typography, controls, or
loading-message settings. Adjacent Full buffers remain in CPU memory. Viewr
uploads Full pixels only for the current zoom view, with visible 1024-pixel
tiles first. Loading status remains tied to required work for the current
image, not adjacent prefetch.

## Benchmark method

The measurements used an Apple M5 MacBook Pro with 10 cores and 24 GB of RAM,
macOS 26.2 arm64, and Rust 1.96.1. The released reference is tag `v0.3.0` at
commit `e76d3a97c`. The selected implementation was measured through commit
`301c79f`.

```sh
cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- navigation_plan --noplot
cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- ram_cache --noplot
cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- full_cache_policy --noplot
```

The released queue result contains the old fixed Full window. The current
fixed-reference column measures that work shape in the new queue code. It is a
bridge that separates general queue changes from the cost of the adaptive
working set. All ranges are Criterion confidence intervals from local runs.
Several sub-microsecond groups reported high outliers. Use these measurements
to judge control-path scale, not as an end-to-end speedup claim.

| Queue synchronization | Released v0.3 fixed | Current fixed reference | Current adaptive |
|---|---:|---:|---:|
| 100 images | 1.08–1.97 µs | 0.787–1.069 µs | 0.931–1.153 µs |
| 1,000 images | 1.74–2.12 µs | 0.820–0.862 µs | 0.958–0.986 µs |
| 10,000 images | 2.05–2.60 µs | 0.841–0.957 µs | 0.968–1.192 µs |

The adaptive queue adds approximately 0.12 to 0.17 µs at the median compared
with the current fixed reference. The complete queue operation remains near
one microsecond. The identity-order algorithm does not scan the complete
folder.

| Planner or filtered queue | 100 | 1,000 | 10,000 |
|---|---:|---:|---:|
| Adaptive identity planner | 110–112 ns | 111–113 ns | 120–263 ns |
| Public planner with 10% filter | 263–285 ns | 1.500–1.557 µs | 11.98–12.73 µs |
| Production queue with 10% filter | 532–616 ns | 0.920–0.988 µs | 0.974–1.257 µs |

The public planner defensively normalizes a supplied filter on every call. Its
linear result at 10,000 images is expected. The engine normalizes the filter
once when the sequence changes, so its navigation path remains near one
microsecond.

| Cache operation | Result |
|---|---:|
| Browse insert with one eviction | 63.2–64.2 ns |
| Replace and refill eight small Full entries | 1.228–1.258 µs |
| Read snapshots with 10,000 observations | 6.99–7.05 ns |
| Change a Browse observation while both 10,000-entry snapshots are live | 25.4–31.4 µs |
| Insert and evict one final 16 MiB Full owner | 1.05–1.52 µs |
| Evict eight final 16 MiB Full owners, 128 MiB total | 5.49–7.94 µs |

The released Browse insertion reference was 47.9–54.2 ns. Recording Browse and
Full size estimates adds approximately 10 ns at the median to this path. A
changed observation can copy two folder-sized maps while it holds the cache
mutex. The 10,000-entry stress result is below 0.032 ms on this computer.
Production holds the snapshots only during the approximately one-microsecond
navigation transaction, which makes this overlap uncommon. The benchmark
remains in the suite to detect growth in this residual contention risk.

Final-owner eviction is also far below one 16.7 ms frame on this allocator.
Navigation changes, insertions, and replacements release the owner after they
unlock the cache, so concurrent cache reads do not wait for allocator work.

These microbenchmarks measure planning, queue mutation, and cache policy. They
do not prove that the 60/20/20 split maximizes real-world cache hits. The RAW
benchmark stayed opt-in because no tracked RAW fixture represents the user's
photo workload.

## Capacity example

The default 4.5 GB setting is 4,500,000,000 bytes. It assigns 402,653,184 bytes
(384 MiB) to thumbnails and leaves 4,097,346,816 bytes for the 60/20/20 policy.
App rounding gives Full RGBA 2,458,408,090 bytes. Browse RGBA and encoded JPEG
each receive 819,469,363 bytes.

The table assumes tightly packed four-byte RGBA pixels and exact observations:

| Sensor size | Approximate Full count |
|---|---:|
| 12 MP | 51 |
| 24 MP | 25 |
| 33 MP | 18 |
| 61 MP | 10 |

The actual count uses each buffer's byte length and the priority-prefix rule.
Mandatory current and adjacent renders can add an overrun. Allocator and cache
metadata are outside payload-byte accounting.

## Correctness and adversarial review

Deterministic tests cover these cases:

- byte budgets at several image sizes and mandatory-set overruns;
- forward, backward, jump, reversal, and filtered navigation;
- invalid and duplicate filtered indices;
- Browse-wave convergence under a small byte budget;
- late completion after a working-set change;
- best-effort A-to-B-to-A reuse and no concurrent same-image decode;
- promotion, demotion, persistence, disk cleanup, and corrupt cache objects;
- optional failure and panic suppression for one navigation wave;
- one-job background limits and fixed one-thread or two-thread serialization;
- concurrent navigation transaction consistency;
- cache-snapshot stability and final-owner eviction outside the mutex;
- zoom urgency from wheel, pinch, mode, and keyboard actions;
- suppression of adjacent Full failures from visible status.

Separate adversarial reviews examined cache and scheduler concurrency, test and
benchmark coverage, and UI behavior. They found no remaining branch-introduced
correctness, deadlock, unsafe-code, arithmetic, platform, or visual blocker.

## Decision

Use the adaptive byte-budget planner and the dedicated Full ring. The design
removes the fixed one-image-ahead limit and evicts stale optional Full pixels
immediately. Its measured control-path cost is small compared with RAW decode
and development.

Keep the 60/20/20 split labeled as experimental. Collect real navigation traces
before claiming that this split is optimal or changing the default again.
