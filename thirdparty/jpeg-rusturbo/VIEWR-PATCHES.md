# Viewr jpeg-rusturbo fork

This directory contains a reviewed fork of `jpeg-rusturbo` 0.9.2.

## Source

- Upstream repository: <https://github.com/naoto256/jpeg-rusturbo>
- Crates.io version: 0.9.2
- Crates.io checksum:
  `f99890ec2a56818f0a1783cd6893794637a4fb6b61a3b4394e411d2f4693372f`
- Imported files: the published crate source, benchmarks, README, changelog,
  licenses, and notice

The fork stays at version 0.9.2 so the dependency graph cannot select both the
upstream crate and this fork. The root workspace and the independent JPEG
bake-off workspace patch crates.io to this directory.

## Viewr changes

Viewr encodes RGBA input as baseline 4:4:4 JPEG. It sets the restart interval
to one MCU row. Each restart row has independent DC predictors and ends on a
byte boundary. The fork uses that property to encode complete rows in parallel
and then joins the byte-stuffed row segments in raster order.

The fast path has these limits:

- baseline RGBA input;
- 4:4:4 sampling;
- more than one MCU row;
- a restart interval that is exactly one MCU row; and
- a thread setting other than one.

All other inputs use the upstream path. The fork does not add public unsafe
code or inline assembly. Existing architecture-specific SIMD stays behind the
upstream dispatch and scalar fallback.

## Correctness contract

For eligible input, every thread count must produce the same bytes as the
single-thread encoder. Tests cover odd and partial MCUs, qualities 1 through
100, automatic and fixed pools, and restart marker wraparound. Viewr also
compares production-size output across its one-thread and multi-thread pools.

The row join rejects an append when the destination bit writer is not empty or
byte-aligned. Parallel workers only read shared pixels and tables. Each worker
owns its predictors, bit writer, and output vector.

## Update procedure

1. Import the next reviewed upstream release.
2. Record its version and crates.io checksum in this file.
3. Reapply the smallest required Viewr patch.
4. Run the fork tests, scalar-fallback tests, Viewr JPEG tests, Miri, clippy,
   and the source-package validation.
5. Compare serial and parallel output bytes on all test cases.
6. Run the no-restart and row-restart benchmark variants. Keep the patch only
   if the end-to-end result is statistically significant.
