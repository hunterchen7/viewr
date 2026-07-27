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

Do not edit `vendor/rawler-0.7.2` directly. Cargo verifies files in that
directory against `.cargo-checksum.json`.

To build Viewr with a changed rawler library:

1. Run `scripts/prepare-local-rawler.sh`.
2. Edit files in `local/rawler-0.7.2`.
3. Run `cargo build --release --locked --offline`.

The preparation script copies rawler to a writable directory, removes the
vendor checksum file, adds an exact local Cargo patch, and updates `Cargo.lock`
without network access.

The project README lists the required Linux packages, including
`build-essential` and `pkg-config`. The macOS build requires Xcode
command-line tools. The Windows build requires the MSVC build tools.
