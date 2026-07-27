# RAW pipeline optimization, 2026-07-27

## Scope

This pass measured the work around JPEG encoding. It did not change the
graphical interface, image dimensions, color rendering, preload policy, cache
quality, or scheduling priorities.

Measurements used:

- Apple M5, 10 CPU cores, 24 GB memory, macOS 26.2
- release/benchmark profile with thin LTO
- a public-domain Sony DSC-RX100 ARW fixture, SHA-256
  `579a485b5126a25cbd55cbd5dadfa7d09cf021c99cc7d4869f9e56e3f759390b`
- Criterion medians unless a result is identified as a diagnostic-run median

The opt-in RAW benchmark does not put the fixture in the repository:

```sh
VIEWR_BENCH_RAW=/path/to/fixture.ARW \
  cargo bench -p viewr-core --bench core_hot_paths \
  --features benchmarks --locked -- raw_opt_in --noplot
```

## Baseline

| Stage | Median |
| --- | ---: |
| Container and RAW decode | 7.283 ms |
| Metadata only | 0.380 ms |
| Browse development | 7.136 ms |
| Full development | 69.893 ms |
| 33 MP 90-degree orientation | 8.129 ms |
| 33 MP 270-degree orientation | 8.038 ms |
| 8 MP cached JPEG decode | 43.076 ms |
| 33 MP cached JPEG decode | 170.420 ms |

Full development is dominated by PPG demosaicing. Diagnostic runs placed that
stage at approximately 66–75 ms. RAW normalization, color calibration, packing,
and orientation are smaller but can still matter to Browse latency.

## Accepted change: fuse integer conversion and CFA normalization

`RawImage::apply_scaling` converts the complete `u16` mosaic to an unscaled
`f32` allocation. It then reads and writes that allocation again to apply the
four Bayer black and white levels. Viewr consumes the decoded RAW, so it never
uses the intermediate unscaled frame.

The new path writes the final normalized `f32` values directly into the output
allocation. Float-backed DNG data continues to use rawler's existing in-place
normalization.

| Scaling implementation | Median | Change |
| --- | ---: | ---: |
| rawler two-pass reference | 2.776 ms | baseline |
| fused production path | 1.768 ms | 36.3% faster |

On the same fixture, Browse development moved from 7.136 ms on clean `main` to
6.287 ms, an 11.9% reduction. Full development remained inside run-to-run
variance because PPG demosaicing is much larger than the approximately 1.0 ms
normalization saving.

Correctness controls:

- exact output comparison with rawler for even and odd sensor dimensions;
- unchanged float-backed normalization path;
- real-Sony Browse and Full development through the existing exact crop/layout
  test;
- strict-provenance Miri validation of the initialized-allocation boundary;
- no new dependency and no cache-version change.

The benchmark retains both the production and legacy scaling functions. This
makes the optimization independently measurable even when full development is
noisy.

## Rejected variants

### Fuse color calibration with RGBA packing

This removed one demosaiced-frame traversal and produced byte-identical output.
It reduced loop efficiency enough to cancel the memory-traffic saving.

| Quality | Clean-main median | Fused median |
| --- | ---: | ---: |
| Browse | 7.136 ms | 7.466 ms |
| Full | 69.893 ms | 72.211 ms |

The change was discarded.

### Replace zune-jpeg cache decoding with jpeg-rusturbo

Both decoders produced the required RGBA layout. jpeg-rusturbo was slower on
this Apple M5 for Viewr's 4:4:4 quality-97 cache objects.

| Cache object | zune-jpeg | jpeg-rusturbo | Result |
| --- | ---: | ---: | ---: |
| Browse, 8 MP | 43.076 ms | 48.134 ms | 11.7% slower |
| Full, 33 MP | 170.420 ms | 191.090 ms | 12.1% slower |

The production decoder remains zune-jpeg.

### Change the Rayon worker count

Ten workers, which matches the available logical CPU count, was the best stable
choice. Diagnostic total-pipeline medians were approximately 237 ms with 10
workers, 240 ms with 8, 261 ms with 6, and 285 ms with 4. No thread cap was
added.

### Share one decoded RAW between Browse and Full jobs

RAW decode costs approximately 7.3 ms. Cloning this fixture's mosaic costs
approximately 3.3 ms, so the upper-bound saving is about 4 ms per Browse/Full
pair.

The current queue can run the two qualities independently and cancel Full work
without delaying Browse. Sharing would require same-image job coordination,
temporary retention of tens of megabytes, and careful cancellation ownership.
That architectural cost is not justified by the measured upper bound in this
pass.

## Follow-up candidates

### PPG demosaicing

Rawler's PPG implementation creates a zero-filled full RGB float frame before
placing each Bayer sample in one channel. It also resolves the repeating CFA
channel in inner loops. Removing that work could be useful, but the
interpolation functions are private to rawler.

Copying and maintaining the complete PPG implementation in Viewr would add a
large unsafe algorithm surface and mix LGPL implementation code into this MIT
repository. The preferred route is a measured optimization in rawler itself,
followed by a pinned dependency update with exact pixel comparisons.

### Cold cache rehydration

Decoding a 33 MP quality-97 4:4:4 JPEG takes about 170 ms on this computer. This
does not affect an RGBA RAM-cache hit, and encoding remains on the separate
persistence lane, but it is material after restart or RAM eviction.

The tested pure-Rust replacement was slower. A future experiment should compare
zune-jpeg, native libjpeg-turbo, and redevelop-from-RAW latency across a
multi-camera corpus. It must include disk size, installer portability, decoder
safety, and the effect on background preloading before changing the cache
format or bypass policy.
