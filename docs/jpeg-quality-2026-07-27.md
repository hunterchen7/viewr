# Cache JPEG-quality experiment — 2026-07-27

## Problem and correctness gate

Cached Browse renders used JPEG quality 87 with 4:2:0 chroma, and cached Full
renders used quality 90 with 4:4:4 chroma. Rehydrating these files could expose
JPEG quantization as contour-like bands in dark gradients and damage subtle
color detail.

The promoted setting must materially reduce dark-gradient error, retain exact
dimensions and opaque-alpha reconstruction, decode on every supported
platform, and remain bounded by the existing RAM and 20 GiB disk-cache budgets.

## Quality variants

A deterministic 1600 by 1067 dark radial gradient includes low-amplitude
sensor-like noise and subtle chroma variation. The gradient-error score is the
mean absolute error in horizontal pixel-to-pixel luminance deltas; lower is
better.

| Variant | Chroma | PSNR | Gradient error | Encoded bytes |
|---|---:|---:|---:|---:|
| q87 legacy Browse | 4:2:0 | 44.799 dB | 1.602 | 93,368 |
| q90 legacy Full | 4:4:4 | 45.139 dB | 1.575 | 150,161 |
| q92 | 4:4:4 | 45.311 dB | 1.554 | 173,153 |
| q95 | 4:4:4 | 45.908 dB | 1.460 | 272,567 |
| q97 | 4:4:4 | 47.144 dB | 1.227 | 419,443 |
| q98 | 4:4:4 | 48.670 dB | 0.938 | 524,392 |
| q100 | 4:4:4 | 52.824 dB | 0.182 | 729,092 |

q95 provides only a modest gradient improvement. q98 and q100 continue to
improve the fixture, but their storage growth is disproportionate. q97 reduces
the gradient-error score by about 22% relative to the legacy Full setting and
is the best measured safe default.

## User-selected quality

The Preferences window exposes the inclusive range q80–q100 and keeps q97 as
the default. The setting applies when the next folder is opened so an active
engine and its RAM cache always use one immutable encoding profile.

Disk keys include every non-default quality. The q97 profile deliberately
retains the version-5 key shape, so existing q97 objects remain reusable.
Changing quality cannot rehydrate an object encoded at another quality.

## Production-shape cost

The existing Criterion suite measures the promoted q97 setting on
production-sized deterministic photo fixtures:

| Work | Legacy median | q97 median |
|---|---:|---:|
| Browse 8 MP encode | 44.47 ms at q87 | 99.02 ms |
| Full 33 MP encode | 251.83 ms at q90 | 389.13 ms |
| Browse 8 MP decode | 18.10 ms | 51.76 ms |
| Full 33 MP decode | 104.51 ms | 187.54 ms |

Command:

```sh
cargo bench -p viewr-core --features benchmarks \
  --bench core_hot_paths --locked -- jpeg --noplot
```

Environment: base merge `56b6a9e`, Rust 1.96.1, macOS arm64, Darwin 25.2.0.
The synthetic-photo probe produced 5.08 MB for Browse and 20.16 MB for Full at
q97, compared with 0.45 MB and 4.11 MB at the legacy settings. These are
deliberately texture-heavy fixtures and not storage predictions for every
camera image.

## Decision and migration

Both cache tiers now use q97 with explicit 4:4:4 chroma. Encoding remains on
the bounded background persistence lane, so the extra work does not extend the
initial RAW develop critical path. Rehydration is slower and the fixed cache
budgets retain fewer images; this is the accepted cost of preventing obvious
lossy-cache artifacts.

The disk-cache develop version advances from 4 to 5. Existing q87/q90 objects
therefore cannot be mistaken for new renders and become eligible for ordinary
cache garbage collection.
