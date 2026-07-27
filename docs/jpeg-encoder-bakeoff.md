# JPEG encoder bake-off — 2026-07-27

## Decision

Viewr now uses original C `libjpeg-turbo` through the safe `turbojpeg` Rust
API. The C source is bundled, built with SIMD required, and statically linked.
It does not depend on Homebrew, a Linux package, or a Windows DLL at runtime.

On the target Apple M5, production-shape q97 encoding is about 65% faster than
the former `jpeg-encoder` path. A real Sony RAW comparison reduced combined
Browse and Full JPEG time by 62.2% while producing byte-for-byte identical
decoded RGB pixels. The selected path also used about half the peak memory and
29% less CPU than the four-thread pure-Rust finalist for the same ten-image
workload.

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
| `libjpeg-turbo-rs` | Pure-Rust reimplementation | 0.7.0 | SIMD, accurate integer DCT |
| `libjpeg-turbo-c` | Original C through `turbojpeg` 1.5.1 | 3.1.0 | NEON, accurate integer DCT |

The C source version is the copy bundled by `turbojpeg-sys` 1.2.0. The locally
installed C 3.1.4.1 library was measured separately and did not outperform the
bundled build. The bundled source was selected for deterministic releases and
cross-platform installer builds.

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

The benchmark search was deliberately bounded to the six variants, four
qualities, four quality fixtures, and two production sizes above.

## Quality and size

At the production q97 setting, all six variants produced the same size and the
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
| `jpeg-encoder` | 95.41 ms | 379.20 ms |
| `jpeg-rusturbo-t1` | 40.59 ms | 161.10 ms |
| `jpeg-rusturbo-t2` | 35.42 ms | 138.27 ms |
| `jpeg-rusturbo-t4` | 32.33 ms | 125.55 ms |
| `libjpeg-turbo-rs` | 35.58 ms | 142.83 ms |
| `libjpeg-turbo-c`, new handle | 30.76 ms | 129.97 ms |
| `libjpeg-turbo-c`, reused handle | 30.46 ms | 121.43 ms |

The C and four-thread Rust paths are effectively tied at q97 wall time. C was
faster on the high-chroma fixture and at q100; four-thread Rust was faster on
some q80 and low-entropy cases. Viewr reuses the C compressor on its
single-threaded persistence lane.

The post-integration production benchmark, which includes a fresh handle in
the public stateless helper, measured:

| Work | Before | After | Change |
|---|---:|---:|---:|
| Browse 8 MP encode | 95.41 ms | 32.39 ms | −66.1% |
| Full 33 MP encode | 379.20 ms | 134.25 ms | −64.6% |
| Browse decode | 46.63 ms | 43.24 ms | −7.3% |
| Full decode | 185.62 ms | 174.33 ms | −6.1% |

The decode improvement is a secondary observation: the decoder did not change,
and different JPEG marker layout can affect its setup cost.

Five alternating runs of the real Sony RAW diagnostic command gave these
combined Browse + Full encode times:

- former path: 308.0 ms median (297.1–320.8 ms);
- selected path: 116.5 ms median (112.0–152.9 ms);
- median reduction: 62.2%.

## CPU, memory, and release cost

Ten Full q97 encodes compared the two wall-time finalists:

| Encoder | Wall | User CPU | Peak RSS | Encoded bytes |
|---|---:|---:|---:|---:|
| `jpeg-rusturbo-t4` | 1.29 s | 1.66 s | 392,085,504 | 201,570,250 |
| bundled `libjpeg-turbo-c` | 1.27 s | 1.19 s | 209,354,752 | 201,570,250 |

The C path avoids a private Rayon pool per encode. This matters because cache
persistence runs beside RAW development and adjacent-image preload work.

The arm64 macOS release binary changed from 26,804,864 to 26,784,256 bytes
(20,608 bytes smaller). `otool -L` reports no JPEG dynamic library, confirming
that the selected C library is statically linked.

The installed system `libjpeg-turbo` 3.1.4.1 completed the same stress workload
in 1.35 s wall, 1.20 s user CPU, and 208,486,400 bytes peak RSS. This single
deployment comparison showed no reason to trade reproducibility for a system
dependency.

## Architecture and safety

The app contains no handwritten `unsafe` and does not call raw C symbols.
`CacheJpegEncoder` owns the safe wrapper handle and stays on the one
persistence thread. It is not exposed as public API.

Before the wrapper sees an image pointer, Viewr checks:

- quality is in 1–100;
- dimensions are non-zero and fit the JPEG 16-bit format limit;
- row-stride and total-size arithmetic cannot overflow;
- RGBA storage length exactly matches width × height × 4.

The persistence worker lazily creates and reuses one compressor. Any native
encode error drops the handle, so one failed request cannot poison every later
cache write. Tests cover cross-decode, explicit 4:4:4, invalid input, quality
bounds, repeated handle use, state isolation between dimensions, and recovery
after a bad request.

Safety validation:

- The vendored C source was compiled with Apple Clang 17 AddressSanitizer.
  Invalid-input, round-trip, and reused-handle tests completed without a
  sanitizer finding.
- Miri passed the pure-Rust pre-FFI validation test.
- Miri cannot execute external C and therefore is not evidence about
  `libjpeg-turbo` internals.
- The Rust wrapper's native boundary was reviewed. Viewr's validation is
  intentionally stricter than its assertion-based image-layout preconditions.

Build and release behavior:

- `pkg-config` is disabled for the production dependency;
- bundled source and SIMD are required;
- Linux CI declares CMake and NASM explicitly;
- macOS, Windows, and Linux release jobs build the same locked dependency;
- the generated third-party license inventory includes `turbojpeg`,
  `turbojpeg-sys`, CMake, and their transitive crates.

The codec switch advances the disk object store to `objects-v7`. Existing
`objects-v6` bytes cannot hide the new performance, and obsolete stores are
removed in the same symlink-safe background cleanup used for the original
legacy store.

## Reproduce

```sh
cargo test --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
cargo run --release --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
cargo bench --manifest-path tools/jpeg-bakeoff/Cargo.toml \
  --locked --bench encode -- --noplot
cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- jpeg --noplot
```

Resource comparison:

```sh
/usr/bin/time -lp tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
  stress libjpeg-turbo-c 10 97
```

## Limits and next trigger

Synthetic data cannot represent every camera, noise profile, or cache-quality
choice. The fixed suite therefore includes entropy and chroma extremes plus a
real developed Sony RAW, but it is not an exhaustive visual study.

Re-run the bake-off when one of these changes:

- the target computer architecture;
- the default cache quality or chroma policy;
- a candidate's SIMD implementation;
- `turbojpeg-sys` bundles a newer original C source;
- the persistence lane becomes concurrent.

`jpeg-rusturbo` with four threads is the measured pure-Rust fallback if native
build or safety costs later become unacceptable.
