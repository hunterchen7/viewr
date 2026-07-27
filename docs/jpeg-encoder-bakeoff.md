# JPEG encoder bake-off — 2026-07-27

## Decision

Viewr now uses pure-Rust `jpeg-rusturbo` in automatic mode, which uses the
ambient Rayon pool sized for the current computer. The app has no new native
library, FFI, or runtime dependency.

On the target Apple M5, production-shape q97 encoding is about 70% faster than
the former `jpeg-encoder` path. A real Sony RAW comparison reduced combined
Browse and Full JPEG time by 61.4% while producing byte-for-byte identical
decoded RGB pixels.

`jpeg-rusturbo` automatic mode won the isolated thin-LTO microbenchmark by
6–9%. Original C was effectively tied on the real RAW and used less than half
the peak memory and about 37% less total CPU. The selection follows the stated
goal of maximum isolated encode speed on this machine; C remains the measured
resource-efficiency alternative if memory or shared-CPU pressure becomes the
priority.

No UI behavior or setting changed. The existing q80–q100 preference, q97
default, 4:4:4 chroma, opaque decode, and content-dependent file sizing remain
the same.

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
stable C wall times of 1.25–1.29 seconds. Viewr reuses the C compressor on its
single-threaded persistence lane.

The post-integration production benchmark measured:

| Work | Before | After | Change |
|---|---:|---:|---:|
| Browse 8 MP encode | 95.41 ms | 27.73 ms | −70.9% |
| Full 33 MP encode | 379.20 ms | 112.99 ms | −70.2% |
| Browse decode | 46.63 ms | 46.52 ms | effectively unchanged |
| Full decode | 185.62 ms | 183.75 ms | effectively unchanged |

The decoder did not change. Decode differences are measurement noise and are
not credited to the encoder change.

Five alternating runs of the real Sony RAW diagnostic command gave these
combined Browse + Full encode times:

- former path: 308.0 ms median (297.1–320.8 ms);
- selected path: 118.8 ms median (108.2–123.7 ms);
- median reduction: 61.4%.

The C integration measured 116.5 ms median (112.0–152.9 ms). The C and
automatic-Rust real-photo results are effectively tied; C is 2.3 ms ahead at
the median while automatic Rust wins both fixed production sizes.

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

Two simultaneous ten-image processes completed in 1.30 seconds median for
automatic Rust and 1.29 seconds for reused C. Their combined total CPU was
about 3.91 and 2.53 seconds respectively. This is a process-level saturation
probe, not a model of Viewr's same-process Rayon scheduling, and it was not
used to claim production contention behavior.

The arm64 macOS release binary changed from 26,804,864 to 26,700,400 bytes
(104,464 bytes smaller). `otool -L` reports no JPEG dynamic library.

The installed system `libjpeg-turbo` 3.1.4.1 completed the same stress workload
in 1.35 s wall, 1.20 s user CPU, and 208,486,400 bytes peak RSS. This single
deployment comparison showed no reason to trade reproducibility for a system
dependency.

## Architecture and safety

The app contains no handwritten `unsafe` and the selected encoder is pure
Rust. Each encode configures 4:4:4 and automatic parallelism locally; the
persistence lane remains single-request-at-a-time while its DCT stage can use
the ambient Rayon pool.

Before the wrapper sees an image pointer, Viewr checks:

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
  image-layout preconditions.

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
```

Resource comparison:

```sh
/usr/bin/time -lp tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
  stress libjpeg-turbo-c-reused 10 97
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
