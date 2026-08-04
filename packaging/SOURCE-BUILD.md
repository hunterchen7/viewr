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

The rawler and jpeg-rusturbo libraries that Viewr builds are reviewed in-tree
forks at `thirdparty/dnglab/rawler` and `thirdparty/jpeg-rusturbo`. Source
archive validation also runs Rawler's library tests from its own locked
workspace, so submodule-only safety tests cannot disappear behind Viewr's path
dependency build.
`Cargo.toml` already patches the crates.io dependencies to those paths, so
their files are directly editable:

1. Edit files in `thirdparty/dnglab/rawler`.
   You can also edit files in `thirdparty/jpeg-rusturbo`.
2. Run `cargo build --release --locked --offline`.

Do not edit other `vendor/` directories. Cargo verifies those against
`.cargo-checksum.json`. Both workspaces — Viewr and the JPEG bake-off —
resolve rawler and jpeg-rusturbo from the same in-tree forks.

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
