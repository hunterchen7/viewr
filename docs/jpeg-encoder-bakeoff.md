# JPEG encoder bake-off — 2026-07-27

## Decision

Viewr now uses pure-Rust `jpeg-rusturbo` on a reusable dedicated pool capped at
ten workers. This keeps persistence out of the foreground RAW-development pool
while maximizing measured JPEG-pipeline speed on the target computer. The app
has no new native library, FFI, or runtime dependency.

On the target Apple M5, production-shape q97 encoding is about 70% faster than
the former `jpeg-encoder` path. A real Sony RAW comparison reduced combined
Browse and Full JPEG time by 57.6% while producing byte-for-byte identical
decoded RGB pixels.

`jpeg-rusturbo` automatic mode remained the isolated thin-LTO winner, narrowly
ahead of the bounded ten-worker path. The app uses a dedicated pool so
background persistence cannot submit work to the foreground Rayon pool. The
selected ten-worker bound won the replicated same-process completion
benchmark. Original C remains the measured low-CPU, low-memory alternative.

No UI behavior or setting changed. The existing q80–q100 preference, q97
default, 4:4:4 chroma, opaque decode, and content-dependent file sizing remain
the same.

### Current processing-limit integration

A later processing-thread preference preserves this dedicated ten-worker pool
in Automatic mode, so the measured default foreground/background isolation
remains unchanged. When the user selects a fixed processing-thread cap, cache
encoding instead runs serially on one worker in the engine-owned processing
pool. This keeps the fixed cap strict and leaves the remaining permitted
workers available for foreground RAW work. JPEG bytes are identical across
both strategies.

## Candidates

| Label | Implementation | Version | Relevant mode |
|---|---|---:|---|
| `jpeg-encoder` | Former pure-Rust baseline | 0.6.1 | SIMD, one thread |
| `jpeg-rusturbo-t1` | Pure Rust | 0.9.2 | NEON, one thread |
| `jpeg-rusturbo-t2` | Pure Rust | 0.9.2 | NEON, two-thread DCT stage |
| `jpeg-rusturbo-t4` | Pure Rust | 0.9.2 | NEON, four-thread DCT stage |
| `jpeg-rusturbo-t8` | Pure Rust | 0.9.2 | NEON, eight-thread DCT stage |
| `jpeg-rusturbo-auto` | Pure Rust | 0.9.2 | Ambient 10-thread Rayon pool |
| `libjpeg-turbo-rs` | Pure-Rust reimplementation | 0.7.0 | SIMD, accurate integer DCT |
| `libjpeg-turbo-c` | Original C through `turbojpeg` 1.5.1 | 3.1.0 | NEON, accurate integer DCT |

The C source version is the copy bundled by `turbojpeg-sys` 1.2.0 in the
experimental workspace. The locally installed C 3.1.4.1 library was measured
separately and did not outperform that bundled build.

Upstream references:

- [`jpeg-rusturbo`](https://github.com/naoto256/jpeg-rusturbo)
- [`libjpeg-turbo-rs`](https://github.com/developer0hye/libjpeg-turbo-rs)
- [Rust `turbojpeg` wrapper](https://github.com/honzasp/rust-turbojpeg)
- [original `libjpeg-turbo`](https://github.com/libjpeg-turbo/libjpeg-turbo)

## Method

The checked-in nested workspace at `tools/jpeg-bakeoff` preserves every
candidate without adding losing codecs to Viewr's release graph. Its lockfile
fixes the experimental dependency set.

All encoders received the same tightly packed RGBA8 input and were required to
emit baseline 4:4:4 JPEG. Every result was decoded with Viewr's independent
`zune-jpeg` decoder and checked for:

- exact width and height;
- opaque reconstructed alpha;
- 4:4:4 SOF sampling factors;
- output size;
- RGB PSNR and maximum channel error;
- horizontal neighbor-delta error, which is sensitive to damaged smooth
  gradients and contouring.

The quality/size probe covered q80, q90, q97, and q100 over:

- an 8 MP photographic synthetic image with gradients, edges, and texture;
- a 2.8 MP dark smooth gradient;
- a 2.8 MP high-chroma edge pattern;
- a 2.8 MP low-entropy image.

Criterion measured q97 at the production Browse size (3504×2336, 8.2 MP) and
Full size (7008×4672, 32.7 MP). The fixed stress workload encoded the Full
fixture ten times and reported wall time, CPU time, and peak RSS.

Environment:

- Apple MacBook Pro, Apple M5;
- 10 CPU cores: 4 performance and 6 efficiency;
- 24 GB RAM;
- macOS arm64, Darwin 25.2.0;
- Rust 1.96.1, LLVM 22.1.2;
- release/benchmark profile with thin LTO.

The nested workspace explicitly enables thin LTO for release and benchmark
profiles; it does not rely on inheriting the root workspace profile. Its C
candidate disables `pkg-config`, requires SIMD, and always builds the bundled
3.1.0 source.

The benchmark search was deliberately bounded to the eight variants, four
qualities, four quality fixtures, and two production sizes above.

## Quality and size

At the production q97 setting, all eight variants produced the same size and the
same decoded quality metrics on every probe fixture:

| Fixture | Bytes | RGB PSNR | Max channel error | Neighbor-delta MAE |
|---|---:|---:|---:|---:|
| Photo 8 MP | 5,078,270 | 41.2304 dB | 15 | 2.492594 |
| Dark gradient | 179,018 | 53.6325 dB | 4 | 0.040530 |
| Chroma edges | 7,609,758 | 40.4039 dB | 10 | 2.812034 |
| Low entropy | 90,188 | 65.2767 dB | 2 | 0.000022 |

At q100, metrics and sizes were also identical. At q80 and q90, the
replacement codecs differed from `jpeg-encoder` by at most 0.042 dB on these
fixtures, with no consistent direction. That difference is immaterial and is
well below the visible improvement from selecting a higher user quality.

The real Sony DSC-RX100 fixture produced the same Browse and Full file sizes
before and after the change: 4,988,520 and 13,893,353 bytes. Container bytes
differ because the encoders order JPEG markers differently, but decoding both
pairs with `djpeg` produced identical PPM bytes and SHA-256 hashes.

This confirms that the speed result does not come from lower quality,
subsampling, or smaller output.

## Latency

Criterion q97 estimates from the candidate run:

| Encoder | Browse 8 MP | Full 33 MP |
|---|---:|---:|
| `jpeg-encoder` | 93.22 ms | 363.99 ms |
| `jpeg-rusturbo-t1` | 39.32 ms | 160.88 ms |
| `jpeg-rusturbo-t2` | 33.48 ms | 133.11 ms |
| `jpeg-rusturbo-t4` | 30.16 ms | 120.06 ms |
| `jpeg-rusturbo-t8` | 28.47 ms | 112.24 ms |
| `jpeg-rusturbo-auto` | 28.13 ms | 109.61 ms |
| `libjpeg-turbo-rs` | 35.09 ms | 141.98 ms |
| `libjpeg-turbo-c`, new handle | 29.93 ms | 120.47 ms |
| `libjpeg-turbo-c`, reused handle | 29.97 ms | noisy; see stress result |

Automatic Rust is the isolated latency winner. The reused-C Full Criterion
sample was rejected as unstable because two severe high outliers widened its
estimate to 128.77–189.17 ms. Five isolated ten-image processes instead gave
stable reused-handle C wall times of 1.25–1.29 seconds.

The post-integration production benchmark measured:

| Work | Before | After | Change |
|---|---:|---:|---:|
| Browse 8 MP encode | 95.41 ms | 28.48 ms | −70.2% |
| Full 33 MP encode | 379.20 ms | 110.65 ms | −70.8% |
| Browse decode | 46.63 ms | 46.52 ms | effectively unchanged |
| Full decode | 185.62 ms | 183.75 ms | effectively unchanged |

The decoder did not change. Decode differences are measurement noise and are
not credited to the encoder change.

Five runs of the final ten-worker real Sony RAW diagnostic command gave these
combined Browse + Full encode times:

- former path: 308.0 ms median (297.1–320.8 ms);
- selected ten-worker path: 130.7 ms median (123.4–142.4 ms);
- median reduction: 57.6%.

The automatic-Rust integration measured 118.8 ms median (108.2–123.7 ms), and
the C integration measured 116.5 ms median (112.0–152.9 ms). The dedicated
ten-worker selection is based on the replicated same-process workload below,
not this noisier serial diagnostic.

### Dedicated-pool worker selection

A follow-up benchmark compared reusable dedicated `jpeg-rusturbo` pools with
4, 8, and 10 workers. It used the same public Sony RAW described below. The RAW
was decoded once outside measurement. The background JPEG input was produced
by a Full development of that RAW, also outside measurement. Each timed
foreground operation was a separate Full development from a pre-cloned RAW
input through Viewr's global Rayon pool. Warmup, input cloning, pool creation,
and background-thread creation were excluded from the samples.

The first exploratory process recorded nine isolated and nine same-process
concurrent samples:

| Dedicated workers | Isolated JPEG | Concurrent JPEG | Concurrent Full develop | Concurrent pair wall |
|---:|---:|---:|---:|---:|
| 4 | 79.049 ms | **126.712 ms** | **88.295 ms** | **126.728 ms** |
| 8 | **74.776 ms** | 132.363 ms | 91.982 ms | 132.377 ms |
| 10 | 75.599 ms | 130.116 ms | 91.751 ms | 130.131 ms |

That one process favored four workers during overlap, but the concurrent
samples were noisy enough that it was not used alone. Five more independent
processes alternated worker order to reduce thermal and ordering bias. The
median across those five process-level, nine-sample medians was:

| Dedicated workers | Isolated JPEG | Concurrent JPEG | Concurrent Full develop | Concurrent pair wall |
|---:|---:|---:|---:|---:|
| 4 | 83.733 ms | 130.430 ms | **82.555 ms** | 130.444 ms |
| 8 | **78.497 ms** | 133.166 ms | 83.080 ms | 133.180 ms |
| 10 | 79.310 ms | **121.089 ms** | 85.183 ms | **121.105 ms** |

The replicated result exposes the actual tradeoff. Ten workers maximized
background completion throughput: its pair median was 7.2% lower than four.
Four workers best protected foreground Full development: its foreground median
was 3.1% lower than ten. Eight was within 0.6% of four on foreground latency
but had the slowest concurrent pair.

The production cap is ten because this optimization explicitly targets maximum
JPEG-pipeline speed on the current computer. This chooses 7.2% faster measured
pair completion over four workers' 3.1% foreground-latency advantage. The JPEG
work remains isolated from the foreground Rayon pool, so this is a deliberate
CPU-contention tradeoff rather than a return to shared-pool scheduling.

Raw samples from the initial process in milliseconds (`isolated JPEG /
concurrent JPEG / concurrent Full develop / concurrent pair`) were:

```text
4:
80.240 81.764 77.732 79.049 77.982 77.697 81.176 79.764 78.264
126.712 124.755 122.283 117.115 161.952 173.235 139.832 126.867 119.998
94.645 88.295 84.600 83.474 113.105 105.464 83.127 96.724 84.267
126.728 124.770 122.298 117.129 161.973 173.246 139.850 126.879 120.012

8:
80.818 72.967 75.892 74.776 73.990 74.033 102.297 74.205 75.760
105.176 102.927 145.274 134.572 143.403 132.363 104.871 125.354 147.088
91.982 89.633 113.974 92.220 96.117 81.180 88.889 82.639 99.941
105.192 102.943 145.291 134.586 143.418 132.377 104.885 125.368 147.214

10:
78.300 74.851 73.744 76.380 75.071 75.599 76.533 76.047 75.143
149.305 130.116 116.229 144.356 133.622 112.871 115.310 135.840 123.827
120.206 89.746 91.301 98.047 91.751 89.460 90.818 97.351 93.475
149.322 130.131 116.244 144.369 133.638 112.884 115.324 135.856 123.839
```

The five alternating-order process medians, in the same four-row order, were:

```text
4:
77.970 81.856 83.733 86.299 85.814
130.430 139.444 128.444 130.042 146.430
81.023 81.409 83.591 82.555 93.037
130.444 139.464 128.459 130.058 146.445

8:
73.687 77.000 78.497 79.902 81.983
119.049 127.014 139.564 133.166 133.988
80.940 86.753 83.080 82.887 86.597
119.062 127.026 139.577 133.180 134.003

10:
74.512 75.695 79.310 80.595 79.518
114.413 121.576 118.800 125.786 121.089
87.887 79.118 87.507 85.183 84.582
114.425 121.589 118.815 125.801 121.105
```

The fixture is the public Sony DSC-RX100 `DSC00838.ARW`, 21,155,328 bytes,
downloaded from `https://raw.pixls.us/data/Sony/DSC-RX100/DSC00838.ARW`.
Its SHA-256 is
`579a485b5126a25cbd55cbd5dadfa7d09cf021c99cc7d4869f9e56e3f759390b`.
The baseline was release commit `7ed6778`; each candidate used a release build.
Runs alternated implementations to limit thermal and background-load bias:

```sh
curl -fL \
  https://raw.pixls.us/data/Sony/DSC-RX100/DSC00838.ARW \
  -o /tmp/DSC00838.ARW
printf '%s  %s\n' \
  579a485b5126a25cbd55cbd5dadfa7d09cf021c99cc7d4869f9e56e3f759390b \
  /tmp/DSC00838.ARW | shasum -a 256 --check -
cargo build --release --locked -p viewr
target/release/viewr dev /tmp/DSC00838.ARW /tmp/viewr-jpeg-output
```

## CPU, memory, and release cost

Ten Full q97 encodes compared the two wall-time finalists:

| Encoder | Median wall | Total CPU | Peak RSS | Encoded bytes |
|---|---:|---:|---:|---:|
| `jpeg-rusturbo-auto` | 1.16 s | 1.95 s | 439,140,352 bytes (418.8 MiB) | 201,570,250 |
| bundled reused `libjpeg-turbo-c` | 1.26 s | 1.23 s | 207,273,984 bytes (197.7 MiB) | 201,570,250 |

Five fresh-process, ten-image q97 stress runs also measured the dedicated-pool
choices:

| Dedicated workers | Median wall | Median total CPU | Median peak RSS | Encoded bytes |
|---:|---:|---:|---:|---:|
| 4 | 1.26 s | 1.77 s | 422,789,120 bytes (403.2 MiB) | 201,570,250 |
| 8 | 1.20 s | 1.96 s | 423,051,264 bytes (403.5 MiB) | 201,570,250 |
| 10 | 1.19 s | 1.98 s | 433,111,040 bytes (413.0 MiB) | 201,570,250 |

Eight and ten workers reduced isolated stress wall time by about 5%, but used
about 11–12% more total CPU. Four and eight had effectively equal median peak
RSS; ten used about 10 MiB more. Output size was identical.

Two simultaneous ten-image processes completed in 1.30 seconds median for
automatic Rust and 1.29 seconds for reused C. Their combined total CPU was
about 3.91 and 2.53 seconds respectively. This is a process-level saturation
probe, not a model of Viewr's same-process Rayon scheduling, and it was not
used to claim production contention behavior.

The arm64 macOS release binary changed from 26,804,864 to 26,721,664 bytes
(83,200 bytes smaller). `otool -L` reports no JPEG dynamic library.

The installed system `libjpeg-turbo` 3.1.4.1 completed the same stress workload
in 1.35 s wall, 1.20 s user CPU, and 208,486,400 bytes peak RSS. This single
deployment comparison showed no reason to trade reproducibility for a system
dependency.

## Production restart-row follow-up — 2026-08-04

The original threaded encoder parallelized color conversion, DCT, and
quantization. It retained all quantized blocks and then ran Huffman emission on
one thread. Viewr already places a restart marker after each 4:4:4 MCU row so
the decoder can split cache objects by row. Those restart rows also make the
encoder DC predictors and byte stream independent at every row boundary.

The reviewed fork at `thirdparty/jpeg-rusturbo` now encodes contiguous groups
of restart rows end to end on the JPEG pool. It joins the byte-stuffed groups
in raster order and writes the completed entropy segment once. The fast path
is limited to Viewr's RGBA, baseline 4:4:4, row-restart shape. Every other
shape uses the upstream path.

An alternating-order, 21-pair Full q97 comparison on the Apple M5 produced:

| Path | Median Full encode | Relative result |
|---|---:|---:|
| Serial row-restart control | 155.945 ms | 1.00× |
| 10-worker restart-row encoder | 27.152 ms | 5.743× faster |

The parallel result was faster in all 21 pairs. A two-sided sign test gives
`p < 0.000001`. Both paths produced exactly 20,157,604 bytes, and every byte
matched. The production Criterion check measured Browse at 8.604 ms and Full
at 28.346 ms. Criterion reported a significant improvement for both tiers.

The first parallel prototype returned one allocation per MCU row. It was fast,
but repeated encodes raised allocator high-water memory. The accepted design
uses one caller-owned output stripe per worker and buffers the ordered entropy
segment before one destination write. A fresh 20-image Full q97 stress run
measured:

| Path | Wall time | Total CPU | Peak RSS | Total encoded bytes |
|---|---:|---:|---:|---:|
| Upstream no-restart threaded path | 2.26 s | 3.75 s | 435,912,704 bytes | 403,140,500 |
| Restart-row threaded path | 0.61 s | 4.15 s | 199,540,736 bytes | 403,152,080 |

The accepted path reduced wall time by 73% and peak RSS by 54%. It used 11%
more total CPU. The small output-size increase is the standard DRI segment and
restart markers; decoded pixels do not change.

Fork-local tests cover odd and partial MCUs, qualities 1, 50, 97, and 100,
automatic and fixed thread pools, RST7-to-RST0 wraparound, and a release-mode
guard against joining a segment while entropy bits are pending. Viewr also
checks byte equality between strict one-thread and production pools on a
1023×769 textured input. The change adds no unsafe code or inline assembly.
[`VIEWR-PATCHES.md`](../thirdparty/jpeg-rusturbo/VIEWR-PATCHES.md) records the
fork source, checksum, contract, and update procedure.

## Architecture and safety

The JPEG integration adds no handwritten `unsafe` or FFI and the selected
encoder is pure Rust. Each encode configures 4:4:4 and automatic parallelism
inside a reusable dedicated pool capped at ten workers. The persistence lane
remains single-request-at-a-time.

Before the encoder sees a pixel slice, Viewr checks:

- quality is in 1–100;
- dimensions are non-zero and fit the JPEG 16-bit format limit;
- row-stride and total-size arithmetic cannot overflow;
- RGBA storage length exactly matches width × height × 4.

Tests cover cross-decode, explicit 4:4:4, invalid input, quality bounds, state
isolation between dimensions, and recovery after a bad request.

Safety validation:

- The losing C candidate was compiled with Apple Clang 17 AddressSanitizer.
  Invalid-input, round-trip, and reused-handle tests completed without a
  sanitizer finding during evaluation.
- Miri passed the pure-Rust input-validation test.
- A full automatic-thread encoder run under Miri stopped in
  `crossbeam-epoch`'s pointer-tagging implementation; this is not counted as
  either a pass or a finding in Viewr or `jpeg-rusturbo`.
- Viewr's validation is intentionally stricter than the selected encoder's
  pixel-layout preconditions.

Build and release behavior:

- macOS, Windows, and Linux release jobs build the same locked pure-Rust
  dependency;
- the app's release graph contains no `turbojpeg` or `turbojpeg-sys`;
- the nested experimental workspace disables `pkg-config` and requires the
  bundled C SIMD build so its C comparison stays reproducible;
- Linux benchmark CI declares CMake and NASM for that experimental candidate.

The codec switch advances the disk object store to `objects-v7`. Existing
`objects-v6` bytes cannot hide the new performance, and obsolete stores are
removed in the same symlink-safe background cleanup used for the original
legacy store.

## Reproduce

```sh
cargo test --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
mkdir -p target
cargo run --release --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
cargo bench --manifest-path tools/jpeg-bakeoff/Cargo.toml \
  --locked --bench encode -- --noplot
cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- jpeg --noplot

for workers in 4 8 10 10 4 8 8 10 4 4 10 8 8 4 10; do
  VIEWR_BENCH_RAW=/tmp/DSC00838.ARW \
    tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
    dedicated-contention "$workers" 9 97
done
```

Resource comparison:

```sh
/usr/bin/time -lp tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
  stress libjpeg-turbo-c-reused 10 97

tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
  restart-compare 10 21 97

/usr/bin/time -lp tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
  stress jpeg-rusturbo-rows-auto 20 97
```

The resource numbers used five fresh processes per finalist:

```sh
for codec in jpeg-rusturbo-auto libjpeg-turbo-c-reused; do
  for run in 1 2 3 4 5; do
    /usr/bin/time -lp \
      tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
      stress "$codec" 10 97
  done
done

for workers in 4 8 10; do
  for run in 1 2 3 4 5; do
    /usr/bin/time -lp \
      tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
      dedicated-stress "$workers" 10 97
  done
done
```

## Limits and next trigger

Synthetic data cannot represent every camera, noise profile, or cache-quality
choice. The fixed suite therefore includes entropy and chroma extremes plus a
real developed Sony RAW, but it is not an exhaustive visual study.

Re-run the bake-off when one of these changes:

- the target computer architecture;
- the default cache quality or chroma policy;
- a candidate's SIMD implementation;
- `jpeg-rusturbo` changes its automatic scheduling or SIMD implementation;
- the persistence lane becomes concurrent.

Bundled original C is the measured low-CPU, low-memory fallback if automatic
parallelism becomes harmful to the rest of the viewer.
