# Viewr JPEG encoder bake-off

This nested workspace compares Viewr's current encoder with three replacement
families without adding the losing candidates to the release dependency graph.
Every case supplies RGBA pixels directly and requires baseline 4:4:4 output.

Run the correctness tests, quality/size probe, and Criterion benchmark:

```sh
cargo test --manifest-path tools/jpeg-bakeoff/Cargo.toml --locked
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
  stress libjpeg-turbo-c 10 97
```

The fixed search budget is:

- `jpeg-encoder` (current baseline)
- `jpeg-rusturbo` with 1, 2, and 4 threads
- `libjpeg-turbo-rs`
- original C `libjpeg-turbo` through the safe `turbojpeg` Rust API
- quality 80, 90, 97, and 100 over photographic, dark-gradient,
  high-chroma-edge, and low-entropy inputs
- quality 97 latency at Viewr's 8 MP Browse and 33 MP Full sizes

This is a machine-specific selection exercise, not a claim that one encoder is
universally fastest. See `docs/jpeg-encoder-bakeoff.md` for the recorded
environment, results, decision, and limitations.
