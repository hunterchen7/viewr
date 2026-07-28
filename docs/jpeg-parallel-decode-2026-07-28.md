# Parallel cache JPEG decode, 2026-07-28

## Scope

This pass attacked the cold cache rehydration cost that the
[RAW pipeline pass](raw-pipeline-optimization-2026-07-27.md) recorded as its
main follow-up: a 33 MP quality-97 4:4:4 cache JPEG took about 170 ms to
decode on one thread. That latency applies after an application restart and
after RAM eviction, on both the RAM-JPEG and the disk tier.

The pass did not change the graphical interface, image dimensions, color
rendering, JPEG quality, cache formats, preload policy, or scheduling
priorities.

Measurements used:

- Apple M5, 10 CPU cores, 24 GB memory, macOS 26.2
- release/benchmark profile with thin LTO
- Criterion medians from the synthetic `jpeg_encode` and `jpeg_decode` groups
- the private Sony ILCE-7CM2 fixture `HCA04875.ARW` for real-content checks

## Baseline

| Operation | Median |
| --- | ---: |
| Encode Browse, 8 MP, q97 | 27.6 ms |
| Encode Full, 33 MP, q97 | 111.4 ms |
| Decode Browse, 8 MP, q97 | 44.1 ms |
| Decode Full, 33 MP, q97 | 171.8 ms |

The decoder was zune-jpeg on one thread. A standard JPEG scan cannot be
decoded in parallel because Huffman symbols have serial bit dependencies and
DC values are differences from the previous MCU.

## Accepted change: row-aligned restart markers and a split decode

The cache encoder now emits a DRI segment whose restart interval is exactly
one MCU row (`ceil(width / 8)` MCUs at 4:4:4). Every restart boundary then
lands on a whole-pixel-row boundary, realigns the bitstream to a byte
boundary, and resets the DC predictors. Each run of MCU rows becomes
independently decodable.

`decode_jpeg` now parses the marker structure, finds the restart positions
with a SIMD byte scan, groups MCU rows into about six byte-balanced chunks
per worker, wraps each chunk in a spec-valid mini JPEG (the original header
with a patched SOF height, the chunk's entropy bytes, and an EOI), and
decodes the chunks on the shared Rayon pool into disjoint slices of one RGBA
allocation.

The bytes on disk stay a standard baseline JPEG. macOS Preview and `sips`
read the new objects unchanged. Existing markerless cache objects stay valid
and keep the serial path, so no cache version bump was needed; new encodes
carry markers and become splittable as the cache naturally rewrites.

| Operation | Before | After | Change |
| --- | ---: | ---: | ---: |
| Decode Browse, 8 MP, q97 | 44.1 ms | 9.2 ms | 79% faster |
| Decode Full, 33 MP, q97 | 171.8 ms | 37.7 ms | 78% faster |
| Encode Browse, 8 MP, q97 | 27.6 ms | 27.7 ms | no significant change |
| Encode Full, 33 MP, q97 | 111.4 ms | 110.8 ms | no significant change |
| Browse object size | 5,078,270 B | 5,078,337 B | +0.001% |
| Full object size | 20,157,025 B | 20,157,604 B | +0.003% |

Encode comparisons had p > 0.05 in both directions across repeated runs. The
size cost is the DRI segment plus two marker bytes and one DC reset per MCU
row.

Correctness controls:

- exact parallel-versus-serial RGBA comparison for synthetic 4:4:4 and 4:2:2
  content at even, odd, and single-MCU-column dimensions;
- exact parallel-versus-serial comparison for real Browse and Full cache
  objects developed from the Sony fixture, in the ignored `real_sony_raw_`
  suite that CI runs with the pinned public-domain fixture;
- a decoded-pixel equality check between marker and markerless encodes;
- fallback tests for markerless, progressive, vertically subsampled,
  single-row, truncated, and interval-corrupted streams;
- chunking invariants (full coverage, no empty chunk) across row counts and
  worker counts;
- the whole-buffer serial decode and the markerless encode remain in the
  benchmark suite as `jpeg_decode_serial` and `jpeg_encode_plain` references.

The split refuses any stream it cannot prove safe and returns to the serial
decoder, so the parallel path can never produce output the serial decoder
would not.

## Rejected variants and limits

### One decode chunk per worker

The first implementation created exactly one chunk per worker. Entropy
density varies between MCU rows, so the join waited on the densest chunk:
33 MP decoded in 41.9 ms and 8 MP in 11.1 ms. Six chunks per worker let work
stealing absorb the imbalance and reached 37.7 ms and 9.2 ms. Three chunks
per worker measured between the two. The knob stops mattering beyond about
six; per-chunk header decode limits further subdivision.

### Vertically subsampled streams

A 4:2:0 split produced pixel differences near chunk seams because fancy
chroma upsampling interpolates across MCU-row boundaries. The splitter
therefore requires a maximum vertical sampling factor of one. Viewr encodes
4:4:4 only, so no production stream is excluded; the test suite pins the
4:2:0 fallback.

### Camera-embedded previews

Camera JPEGs carry no restart markers, so embedded-preview decode cannot use
the split. No change is possible there without transcoding, which would cost
more than it saves.

## Remaining candidates after this pass

PPG demosaicing still dominates Full development (about 66–75 ms of the
~70 ms develop). The interpolation internals are private to rawler and the
[RAW pipeline pass](raw-pipeline-optimization-2026-07-27.md) already
documents why vendoring them is rejected; the preferred route remains a
measured optimization in rawler followed by a pinned update. No further
local candidate above the review thresholds was identified in this pass.
