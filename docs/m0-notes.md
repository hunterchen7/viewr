# M0 spike results — 2026-07-20

Machine: Apple M4 (10-core, 24GB). Release build.
File: Sony A7C II (ILCE-7CM2), 33MP lossless-compressed ARW, 49MB
(`HCA04696.ARW`, 7168×5120 sensor → 7008×4672 delivered).

## Timings (`viewr dev`)

| Stage | Time | Notes |
|---|---|---|
| open + parse | 37ms | file read + TIFF IFD walk + decoder setup |
| metadata-only pass | 13ms | powers the folder-scan MetaScan job |
| entropy decode (lossless) | 76ms | far under the planned 150–400ms |
| browse develop (superpixel, 3504×2336) | 122ms | demosaic 51ms, rescale 37ms¹, calibrate 4ms, pack 16ms |
| full develop (PPG, 7008×4672) | 389ms | demosaic 160ms, pack 82ms |
| JPEG encode both tiers (q87 + q90) | 362ms | background P3 cost per image |

Cold path to browse-ready ≈ **235ms**; to full-res ≈ **500ms**.

¹ browse-vs-full rescale asymmetry (37ms vs 7ms) is first-touch page
faults on the cloned mosaic; the real pipeline decodes per job, no clone.

## Prefetch calibration (from these numbers)

- 3 heavy workers ⇒ ~8–12 img/s browse-tier prefetch throughput at 33MP —
  comfortably ahead of held-arrow navigation.
- Full-tier pre-warm ~0.4s/image ⇒ current±1 full pre-warm is cheap.
- Warm JPEG re-inflate estimate (33MP q90 zune-jpeg): to be measured at M2.

## Correctness

- Colors/WB verified visually against the scene (natural sky/foliage).
- Output is slightly flat vs the camera JPEG — expected: base tone curve
  stage not yet implemented (only sRGB gamma). Planned develop stage.
- Lens description resolved to none for this file (`lens: None`) — revisit
  in M3 metadata panel (fall back to EXIF LensModel string).
- Orientation not yet applied (M1).

## Environment note: olares-sync interop

Hunter's `olares-sync` uploads from `~/Pictures` filtered to an extension
allowlist (raws, images, videos, `xmp`). Consequences:

- viewr's disk cache (`~/Library/Caches/viewr/`) is outside the synced
  tree — never uploaded. Keep it that way; never cache inside photo dirs.
- `.xmp` sidecars WILL sync (almost certainly desirable — ratings are the
  work product). Our atomic-write temp files (`*.tmp`) are not in the
  allowlist, so half-written sidecars can't upload.
- **Caveat found:** olares-sync dedups by remote *byte size*. Editing a
  rating in an existing sidecar usually changes content but not length
  (`Rating="3"` → `"4"`), so the server will keep the stale version.
  That's a property of the sync tool, not viewr. Flagged to Hunter.
