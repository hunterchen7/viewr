# Viewr JPEG encoder bake-off

This nested workspace compares Viewr's former encoder with three replacement
families without adding the losing candidates to the release dependency graph.
Every case supplies RGBA pixels directly and requires baseline 4:4:4 output.
The original C candidate requires CMake; x86 hosts also require NASM for SIMD.

Run the correctness tests, quality/size probe, and Criterion benchmark:

```sh
cargo test --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
mkdir -p target
cargo run --release --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked \
  > target/jpeg-bakeoff.csv
cargo bench --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked --bench encode -- --noplot
```

The probe cross-decodes every output through Viewr's `zune-jpeg` decoder. It
reports output size, median wall time, RGB PSNR, maximum channel error, and
neighbor-delta error. The last metric is sensitive to damaged smooth gradients
and visible contouring. Criterion is the authoritative latency measurement.
The benchmark separately measures a reused C compressor handle to expose any
per-call FFI setup cost. A fixed full-resolution stress workload is available
for CPU and peak-memory measurement:

```sh
/usr/bin/time -lp tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
  stress libjpeg-turbo-c-reused 10 97
```

The selected Rust encoder can also be tested in a reusable dedicated Rayon
pool. The contention command prints every raw sample and a median summary. If
`VIEWR_BENCH_RAW` is set, it decodes the RAW once, builds the JPEG input from a
Full development outside the timed region, and overlaps each background encode
with another Full development of that RAW. Without the variable, it falls back
to overlapping the synthetic 33 MP encode with a 90-degree orientation:

```sh
cargo build --release --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked

for workers in 4 8 10 10 4 8 8 10 4 4 10 8 8 4 10; do
  VIEWR_BENCH_RAW=/tmp/DSC00838.ARW \
    tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
    dedicated-contention "$workers" 9 97
done

for workers in 4 8 10; do
  for run in 1 2 3 4 5; do
    /usr/bin/time -lp \
      tools/jpeg-bakeoff/target/release/viewr-jpeg-bakeoff \
      dedicated-stress "$workers" 10 97
  done
done
```

Fixture decoding, input cloning, fixture development, warmup, pool creation,
and worker creation are outside recorded intervals. The background thread and
its dedicated pool are reused for all samples. Foreground development uses
Viewr's normal global Rayon pool, matching the scheduling interaction that the
dedicated encoder pool is intended to isolate.

The fixed search budget is:

- `jpeg-encoder` (former baseline)
- `jpeg-rusturbo` with 1, 2, 4, and 8 private threads, plus its automatic
  ambient-Rayon-pool mode
- `libjpeg-turbo-rs`
- original C `libjpeg-turbo` through the safe `turbojpeg` Rust API
- quality 80, 90, 97, and 100 over photographic, dark-gradient,
  high-chroma-edge, and low-entropy inputs
- quality 97 latency at Viewr's 8 MP Browse and 33 MP Full sizes

This is a machine-specific selection exercise, not a claim that one encoder is
universally fastest. See `docs/jpeg-encoder-bakeoff.md` for the recorded
environment, results, decision, and limitations.
