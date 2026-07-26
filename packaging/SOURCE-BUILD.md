# Build the released source

The release source archive contains Viewr and its vendored Rust dependencies.
The archive does not require Cargo to download package source.

1. Install the Rust version in `rust-toolchain.toml`.
2. Install the native build dependencies for your operating system.
3. Run `cargo build --release --locked --offline`.

The executable is in `target/release`. On Windows, the file name is
`viewr.exe`.

To test a changed rawler library, change its files in `vendor/rawler-0.7.2`.
Then run the build command again. Cargo uses the vendored source configuration
in `.cargo/config.toml`.

The project README lists the required Linux packages. The macOS build requires
Xcode command-line tools. The Windows build requires the MSVC build tools.
