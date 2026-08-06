# Platform-specific performance audit

Date: 2026-08-05

Audited base: `9faffb752be468cdf9283166c9f369ea008b3286`

Audit branches:

- Viewr: `codex/platform-specific-performance`
- Rawler fork: `codex/platform-specific-performance`

Audited Rawler revision:
`76bd24eb54e76606013ed7f4bdcf5d6b2be2a142`.

## Scope and invariant

This phase audited Rawler and jpeg-rusturbo architecture dispatch, compiler
profiles, baseline coverage, and hosted benchmark coverage. It did not change
Viewr UI code or behavior. The work first closed an unsafe-input path, then
changed dispatch requirements only where emitted code and controlled
measurements supported the change.

The local reference host was an Apple M5 with 10 logical CPUs and 24 GiB of
memory, running ARM64 macOS 26.2 with Rust 1.96.1 and LLVM 22.1.2. Benchmark
and release profiles used thin LTO.

## Correctness boundary

Rawler's safe lossless-JPEG encoder constructor previously accepted zero-sized
images and could pass them to unchecked encoder kernels. It also performed
dimension arithmetic without checked operations. The constructor now rejects
zero or out-of-range dimensions, component counts outside 1 through 4, point
transforms that are not smaller than the bit depth, overflowing row and image
layouts, and undersized input buffers before encoding begins.

The production and forced-baseline release suites each pass 97 tests, with one
ignored test. The added layout regression covers zero dimensions, invalid
component counts and point transforms, and arithmetic overflow.

## Dispatch changes

The audit covered all 35 active Rawler `multiversion` sites. Each site now
supports a default-off forced-baseline build for CI and benchmark comparison.
Production builds retain automatic dispatch.

The Fuji bit reader now treats BMI2 and LZCNT as independent x86-64
capabilities. Its clones cover BMI2 plus LZCNT, BMI2 alone, LZCNT alone, the
x86 SSE baseline, and AArch64 NEON. It does not require AVX because the emitted
Fuji code does not use AVX instructions. LZCNT is a distinct extension; BMI1
does not imply it.

The lossless-JPEG difference kernel retains AVX2 clones with optional BMI2.
It no longer requires FMA or BMI1, because neither capability is used by the
emitted kernel. The portable x86 build still has its SSE baseline, and the
AArch64 build has its NEON baseline.

An x86-64 Linux assembly build confirmed the intended boundaries:

- LZCNT clones contain `lzcnt`; non-LZCNT Fuji clones use `bsr`.
- BMI2 Fuji clones contain `shrx`, `shlx`, and `bzhi` operations.
- Lossless-JPEG AVX2 clones use AVX2 without an FMA or BMI1 requirement.
- Lossless-JPEG AVX2 plus BMI2 clones retain the BMI2 scalar-tail operations.

No handwritten assembly or AVX-512 path was added. LLVM already emits the
required instructions for the accepted tiers, and this phase found no measured
compiler-code-generation defect that would justify a new unsafe boundary.

## JPEG backend audit

jpeg-rusturbo already has an AArch64 NEON backend, an x86-64 backend with AVX2
main kernels and an SSE2 bitmap kernel, and scalar fallbacks. The audit found
no unguarded x86 target-feature call: every AVX2 entry is protected by
`is_x86_feature_detected!("avx2")`, and SSE2 is part of the x86-64 baseline.
NEON is part of the AArch64 baseline.

The existing `force-scalar` feature selects a scalar backend in a separate
whole-build policy. It cannot switch one binary at runtime, and Cargo feature
unification makes it unsuitable for a production dependency graph. CI now
lints the default-feature production backend before its all-feature pass and
release-tests both automatic and forced-scalar builds on macOS ARM64 and
Windows x64. The benchmark header now reports whether an x86 host actually has
AVX2 instead of inferring AVX2 from the compile target.

Direct architecture tests now call the private fused JPEG MCU front halves and
compare their complete coefficient blocks with the scalar reference. They
cover NEON and AVX2 4:4:4, 4:2:2, and 4:2:0 paths, plus the paired AVX2 4:4:4
path, across black, white, primary-color, and deterministic pseudorandom panels
at qualities 25, 80, and 100. Every direct AVX2 call remains runtime-gated.

The audit did not have a controlled pre-AVX2 x86 runner. `force-scalar` tests
the fallback implementation but bypasses the automatic x86 dispatcher, so an
automatic non-AVX2 runtime remains a documented coverage gap.

## Forced-baseline semantics

The Rawler build script recognizes `RAWLER_FORCE_BASELINE=1` and selects a
private CI- and benchmark-only build configuration. It disables optional
function multiversioning for the complete Rawler build. It does not select a
backend at runtime, and it does not force scalar execution. The release
baseline is SSE2 on x86-64 and NEON on AArch64.

This private configuration is not a Cargo feature, so dependency feature
unification cannot disable production dispatch. The build script rejects any
value other than `1`. Use separate Cargo invocations and target directories
for controlled automatic and forced-baseline comparisons. Leave the variable
unset for every production Viewr build.

```sh
cargo test \
  --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
  --lib --release --locked

env CARGO_TARGET_DIR=/tmp/viewr-rawler-force-baseline \
  RAWLER_FORCE_BASELINE=1 \
  cargo test \
    --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
    --lib --release --locked
```

Rawler's standalone release and benchmark profiles now use thin LTO with no
debug information. This matches the shipped Viewr profile and prevents a
standalone fat-LTO benchmark from being compared with a thin-LTO application
build.

## Apple ARM64 Rawler experiments

The thin-LTO automatic-dispatch run produced these Criterion time estimates.
Brackets contain the lower bound, point estimate, and upper bound:

| Workload | Automatic dispatch |
| --- | ---: |
| LJPEG encode, 3000 by 2000 | `[27.806, 28.005, 28.231] ms` |
| LJPEG copy, 3000 by 2000 | `[377.58, 380.67, 384.31] us` |
| Four-channel bilinear, 2048 by 1366 | `[2.1742, 2.2071, 2.2413] ms` |

The separate forced-baseline encode run measured
`[28.252, 28.542, 28.882] ms`. Its change interval included zero. The
automatic-versus-forced comparison returned `p = 0.48` for encode, `p = 0.43`
for copy, and `p = 0.86` for bilinear. None was statistically significant.
This is expected on AArch64, where NEON is part of the architecture baseline.

A separate pre-profile `target-cpu=native` experiment measured the native
encode candidate at `[25.452, 25.508, 25.592] ms`. Its comparison returned
`p = 0.19`; copy returned `p = 0.39`, and bilinear returned `p = 0.37`.
The experiment found no significant improvement, so release builds remain
portable. These pre-profile values must not be compared directly with the
thin-LTO values above.

## Apple ARM64 JPEG decode experiment

An initial Viewr Criterion comparison appeared to favor the scalar build in
some JPEG decode cases. That interpretation was invalid: Viewr's timed decode
groups call zune-jpeg. jpeg-rusturbo only creates their fixture before the
timed loop. The groups are now named `zune_jpeg_decode` and
`zune_jpeg_decode_serial`, and the automatic-versus-scalar Criterion comparison
is restricted to JPEG encode groups.

A direct jpeg-rusturbo experiment then ran two counterbalanced A-B-B-A cycles
with reversed section order. Each policy and case had four process samples;
each process measured 50 single-threaded decodes after three warmups. The
separate thin-LTO binaries used blank `RUSTFLAGS`.

NEON beat forced scalar in all 18 synthetic and natural cases. Scalar divided
by NEON ranged from 1.417 to 4.208. The geometric-mean NEON speedup was 1.797x
for synthetic input, 3.647x for natural-like input, and 2.560x overall.
Same-binary kernel checks measured 5.71x for YCbCr-to-RGB, 12.73x for fancy
horizontal upsampling, and 1.03x for vertical blending. The AArch64 NEON decode
policy remains unchanged.

## Decisions

| Proposal | Decision | Evidence |
| --- | --- | --- |
| Validate safe LJPEG encoder layouts | Accept | Prevents invalid safe inputs from reaching unchecked kernels; regression passes in both builds. |
| Match standalone Rawler profiles to Viewr | Accept | Makes isolated measurements representative of the shipped thin-LTO profile. |
| Separate Fuji BMI2 and LZCNT tiers | Accept | Matches the instructions emitted by each clone and avoids unrelated AVX requirements. |
| Remove LJPEG FMA and BMI1 requirements | Accept | Emitted AVX2 code uses neither capability; optional BMI2 remains used. |
| Add a private forced-baseline build | Accept for separate CI and benchmark builds only | Exercises the architecture baseline across all 35 active multiversion sites without changing Cargo feature semantics. |
| Exercise jpeg-rusturbo automatic and scalar policies | Accept | Both release suites pass on Apple ARM64; Linux and Windows cross-builds compile, and hosted comparisons retain exact artifacts. |
| Keep jpeg-rusturbo NEON decode on AArch64 | Accept | NEON won all 18 counterbalanced direct decode cases, with a 2.560x geometric-mean speedup. |
| Set `target-cpu=native` for releases | Reject | The Apple M5 comparison found no significant improvement and would reduce portability. |
| Add inline assembly | Reject for this phase | No measured LLVM defect or end-to-end benefit justified the added safety and maintenance cost. |
| Add AVX-512 clones | Reject for this phase | No representative workload and host comparison demonstrated a benefit. |
| Hoist Fuji dispatch outside the per-code reader | Defer | It could remove an indirect call per code, but it requires a real Fuji fixture and physical x86 measurement first. |

## Validation and hosted coverage

The following local gates passed:

```sh
cargo test \
  --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
  --lib --release --locked
env CARGO_TARGET_DIR=/private/tmp/viewr-rawler-force-baseline \
  RAWLER_FORCE_BASELINE=1 \
  cargo test \
    --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
    --lib --release --locked
cargo check \
  --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
  --all-targets --all-features --locked
env CARGO_TARGET_DIR=/private/tmp/viewr-check-windows \
  cargo check \
    --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
    --target x86_64-pc-windows-msvc --lib --release --locked
env CARGO_TARGET_DIR=/private/tmp/viewr-codegen-x86 \
  CARGO_PROFILE_RELEASE_LTO=false \
  cargo rustc \
    --manifest-path thirdparty/dnglab/rawler/Cargo.toml \
    --target x86_64-unknown-linux-gnu --lib --release --locked \
    -- -C codegen-units=1 --emit=asm
```

Both release modes reported 97 passed and one ignored. The x86-64
Linux and Windows MSVC cross-checks compiled successfully. The Linux assembly
output was then inspected for the tier-specific instructions listed above.
The layout regression also passed under Miri with strict provenance and tree
borrows in the private forced-baseline build.

jpeg-rusturbo's automatic and forced-scalar Apple ARM64 release suites each
reported 92 unit tests passed and four ignored; eight doctests also passed.
Default and forced test builds compiled for x86-64 Linux, and both benchmark
builds compiled for Windows MSVC. No architecture-tier correctness blocker was
found.

CI uses a consolidated three-row platform matrix: Linux x64, macOS 15 ARM64,
and Windows 2025 x64. Each row uses an explicit Rust target and matching
`RUSTFLAGS`; the Windows row also runs the Rawler forced-baseline release
suite. The separate Linux quality job remains the complete correctness gate.

The advisory benchmark workflow runs manually and weekly on the same three
architectures. Linux runs the broad Viewr suite and JPEG codec bake-off;
macOS and Windows run the architecture-sensitive Viewr paths. Every row uses
the pinned public Sony RAW fixture, compares jpeg-rusturbo automatic and
forced-scalar policies, and compares separate Rawler automatic and
forced-baseline builds. It records the OS, CPU, memory, target, flags,
compiler-reported native features, profile, lockfile hashes, and submodule
revision in a platform-specific artifact. The workflow has no numeric timing
gate; hosted-runner results require reviewer interpretation.
