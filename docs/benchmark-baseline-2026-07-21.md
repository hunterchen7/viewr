# Synthetic benchmark reference — 2026-07-21

This run verifies the first Criterion harness.
It is a reference result, not a performance limit.

## Environment

- Hardware: Apple M5, 10 CPU cores, and 24 GB memory.
- Operating system: macOS 26.2 on arm64.
- Rust: 1.96.1 with LLVM 22.1.2.
- Profile: `bench` with thin LTO.
- RAW fixture: Not set.

Command:

```sh
cargo bench -p viewr-core --features benchmarks --bench core_hot_paths --locked -- --noplot
```

## Representative estimates

| Benchmark | Input | Estimate |
|---|---:|---:|
| Navigation plan | 10,000 images | 21.61 µs |
| Navigation plan with disk-warm targets | 10,000 images | 38.28 µs |
| Navigation plan with 10 percent filter | 10,000 images | 3.87 µs |
| Outward order | 1,000,000 images | 710.22 µs |
| Resize to 2,048 px | 4,032 × 3,024 RGBA | 16.57 ms |
| Resize to 360 px | 4,032 × 3,024 RGBA | 9.46 ms |
| Rotate 90 degrees | 4,032 × 3,024 RGBA | 12.20 ms |
| JPEG encode, quality 80 | 4,032 × 3,024 RGBA | 47.94 ms |
| JPEG encode, quality 92 | 4,032 × 3,024 RGBA | 110.17 ms |
| JPEG decode, quality 88 source | 4,032 × 3,024 RGBA | 21.61 ms |
| RAM RGBA hit | 32 resident entries | 14.52 ns |
| RAM JPEG hit | 32 resident entries | 15.46 ns |
| RAM insert with eviction | 8-entry budget | 114.53 ns |
| XMP attribute parse | 10 KB sidecar | 319.61 ns |
| XMP late-element parse | 10 KB sidecar | 11.83 µs |
| XMP attribute update | 10 KB sidecar | 38.61 µs |

Some groups changed by more than 10 percent between two adjacent runs.
This variation confirms that one local run is not a regression decision.
Save a named baseline before you change code.
Repeat the comparison after you change code.
