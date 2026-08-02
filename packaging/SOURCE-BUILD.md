# Build the released source

The release source archive contains Viewr and its vendored Rust dependencies.
The archive does not require Cargo to download package source.

1. Install the Rust version in `rust-toolchain.toml`.
2. Install the native build dependencies for your operating system.
3. On macOS or Linux, run `cargo build --release --locked --offline`.

On Windows, use a static Microsoft C runtime:

```powershell
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --release --locked --offline
```

The executable is in `target/release`. On Windows, the file name is
`viewr.exe`.

The rawler library that Viewr builds is the in-tree fork at
`thirdparty/dnglab/rawler`. `Cargo.toml` already patches the crates.io
dependency to that path, so its files are directly editable:

1. Edit files in `thirdparty/dnglab/rawler`.
2. Run `cargo build --release --locked --offline`.

Do not edit other `vendor/` directories. Cargo verifies those against
`.cargo-checksum.json`; `vendor/rawler-0.7.2` in this archive serves only
the vendored JPEG bake-off workspace.

The project README lists the required Linux packages, including
`build-essential` and `pkg-config`. The macOS build requires Xcode
command-line tools. The Windows build requires the MSVC build tools.

## Reproduce the JPEG bake-off

The archive also vendors the independent JPEG benchmark workspace. CMake is
required for its original-C candidate. NASM is also required on x86 hosts.
Source-only benchmark dependencies keep their license files beside their
vendored source. The generated third-party inventory describes the shipped
Viewr binary dependency graph, not these experimental candidates.

```sh
cargo test \
  --manifest-path tools/jpeg-bakeoff/Cargo.toml \
  --locked \
  --offline
cargo bench \
  --manifest-path tools/jpeg-bakeoff/Cargo.toml \
  --locked \
  --offline \
  --bench encode -- --noplot
```
