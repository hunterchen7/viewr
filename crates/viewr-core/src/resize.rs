//! SIMD downscaling and orientation rotation for PixelBuf.

use fast_image_resize as fir;
use rayon::prelude::*;

use crate::types::{Orient, PixelBuf};

#[derive(Debug, thiserror::Error)]
/// Failure while constructing or resizing an RGBA image.
pub enum ResizeError {
    /// `fast_image_resize` rejected the dimensions, storage, or resize.
    #[error("resize: {0}")]
    Fir(String),
}

/// Downscale so the long edge is at most `max_edge`, preserving aspect.
/// Returns the input unchanged if it already fits.
/// The no-op path does not validate that RGBA storage matches the dimensions.
///
/// # Errors
///
/// When resizing is required, returns [`ResizeError::Fir`] if source RGBA
/// storage is malformed or the resize operation fails.
pub fn downscale_to_fit(buf: PixelBuf, max_edge: u32) -> Result<PixelBuf, ResizeError> {
    let long = buf.width.max(buf.height);
    if long <= max_edge {
        return Ok(buf);
    }
    let scale = max_edge as f64 / long as f64;
    let dst_w = ((buf.width as f64 * scale).round() as u32).max(1);
    let dst_h = ((buf.height as f64 * scale).round() as u32).max(1);
    resize_exact(buf, dst_w, dst_h)
}

/// Resizes an RGBA8 buffer to exact dimensions with a Catmull-Rom filter.
///
/// Alpha is not interpreted during filtering because pipeline images are
/// opaque. The input allocation is consumed and the output remains tightly
/// packed RGBA8.
///
/// # Errors
///
/// Returns [`ResizeError::Fir`] when the source storage does not match its
/// dimensions or `fast_image_resize` rejects the operation.
pub fn resize_exact(buf: PixelBuf, dst_w: u32, dst_h: u32) -> Result<PixelBuf, ResizeError> {
    let src =
        fir::images::Image::from_vec_u8(buf.width, buf.height, buf.rgba, fir::PixelType::U8x4)
            .map_err(|e| ResizeError::Fir(e.to_string()))?;
    let mut dst = fir::images::Image::new(dst_w, dst_h, fir::PixelType::U8x4);
    let options = fir::ResizeOptions::new()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom))
        .use_alpha(false);
    fir::Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|e| ResizeError::Fir(e.to_string()))?;
    Ok(PixelBuf {
        width: dst_w,
        height: dst_h,
        rgba: dst.into_vec(),
    })
}

/// Rotates a buffer to display orientation, consuming its allocation.
///
/// [`Orient::R0`] is allocation-free and [`Orient::R180`] reverses pixels in
/// place. Quarter turns allocate a destination and exchange dimensions.
///
/// # Panics
///
/// Malformed storage can panic: [`Orient::R180`] checks four-byte divisibility
/// in debug builds, while a quarter turn can index beyond a short buffer.
/// Callers must preserve [`PixelBuf`]'s storage invariant.
pub fn apply_orient(buf: PixelBuf, orient: Orient) -> PixelBuf {
    match orient {
        Orient::R0 => buf,
        Orient::R180 => {
            let mut rgba = buf.rgba;
            let px: &mut [[u8; 4]] = bytemuck_cast(&mut rgba);
            px.reverse();
            PixelBuf {
                width: buf.width,
                height: buf.height,
                rgba,
            }
        }
        Orient::R90 | Orient::R270 => {
            let (sw, sh) = (buf.width as usize, buf.height as usize);
            let (dw, dh) = (sh, sw);
            let src = buf.rgba;
            if dw == 0 || dh == 0 {
                return PixelBuf {
                    width: dw as u32,
                    height: dh as u32,
                    rgba: src,
                };
            }
            let mut dst = vec![0u8; src.len()];
            let rotate_row = |yd: usize, row: &mut [u8]| {
                for (xd, out) in row.chunks_exact_mut(4).enumerate() {
                    let (xs, ys) = match orient {
                        Orient::R90 => (yd, sh - 1 - xd),
                        _ => (sw - 1 - yd, xd),
                    };
                    let i = (ys * sw + xs) * 4;
                    out.copy_from_slice(&src[i..i + 4]);
                }
            };
            if sw.saturating_mul(sh) >= 256 * 256 {
                // A destination row walks one source column, which is a
                // cache-hostile stride for large photos. Rotate bands of
                // destination rows instead: every source-row visit then
                // reads a compact horizontal run while the band's output
                // working set stays small enough for cache.
                const BAND_ROWS: usize = 16;
                let row_bytes = dw * 4;
                dst.par_chunks_mut(row_bytes * BAND_ROWS)
                    .enumerate()
                    .for_each(|(band_index, band)| {
                        let first_yd = band_index * BAND_ROWS;
                        let band_rows = band.len() / row_bytes;
                        for ys in 0..sh {
                            let xd = match orient {
                                Orient::R90 => sh - 1 - ys,
                                Orient::R270 => ys,
                                _ => unreachable!(),
                            };
                            for local_yd in 0..band_rows {
                                let yd = first_yd + local_yd;
                                let xs = match orient {
                                    Orient::R90 => yd,
                                    Orient::R270 => sw - 1 - yd,
                                    _ => unreachable!(),
                                };
                                let src_offset = (ys * sw + xs) * 4;
                                let dst_offset = (local_yd * dw + xd) * 4;
                                band[dst_offset..dst_offset + 4]
                                    .copy_from_slice(&src[src_offset..src_offset + 4]);
                            }
                        }
                    });
            } else {
                dst.chunks_exact_mut(dw * 4)
                    .enumerate()
                    .for_each(|(yd, row)| rotate_row(yd, row));
            }
            PixelBuf {
                width: dw as u32,
                height: dh as u32,
                rgba: dst,
            }
        }
    }
}

/// View a byte vec as pixel quads without unsafe.
fn bytemuck_cast(rgba: &mut [u8]) -> &mut [[u8; 4]] {
    // SAFETY: every bit pattern is valid for `[u8; 4]`, whose alignment is one.
    // The pixel-buffer invariant makes the byte length a multiple of four.
    let (head, mid, tail) = unsafe { rgba.align_to_mut::<[u8; 4]>() };
    debug_assert!(head.is_empty() && tail.is_empty());
    mid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pix(vals: &[u8]) -> PixelBuf {
        // 2x1 image, pixels A=[1,1,1,1], B=[2,2,2,2] style helpers below.
        PixelBuf {
            width: 2,
            height: 1,
            rgba: vals.to_vec(),
        }
    }

    fn labelled(width: u32, height: u32) -> PixelBuf {
        let rgba = (1..=width * height)
            .flat_map(|value| [value as u8, value as u8, value as u8, 255])
            .collect();
        PixelBuf {
            width,
            height,
            rgba,
        }
    }

    fn labels(buf: &PixelBuf) -> Vec<u8> {
        buf.rgba.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    #[test]
    fn rotate_90_cw() {
        // 2x1: [A B] → 1x2: [A] over [B]? For 90 CW, top row becomes right
        // column: A(0,0)→(0,0)? Actual: (x,y)→(h-1-y, x): A(0,0)→(0,0), B(1,0)→(0,1).
        let out = apply_orient(pix(&[1, 1, 1, 1, 2, 2, 2, 2]), Orient::R90);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba, &[1, 1, 1, 1, 2, 2, 2, 2]);
    }

    #[test]
    fn rotate_270_cw() {
        // (x,y)→(y, w-1-x): A(0,0)→(0,1), B(1,0)→(0,0).
        let out = apply_orient(pix(&[1, 1, 1, 1, 2, 2, 2, 2]), Orient::R270);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba, &[2, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn rotate_180() {
        let out = apply_orient(pix(&[1, 1, 1, 1, 2, 2, 2, 2]), Orient::R180);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba, &[2, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn rectangular_rotations_have_expected_layout() {
        let source = labelled(3, 2); // [1 2 3] / [4 5 6]

        let r90 = apply_orient(source.clone(), Orient::R90);
        assert_eq!((r90.width, r90.height), (2, 3));
        assert_eq!(labels(&r90), vec![4, 1, 5, 2, 6, 3]);

        let r180 = apply_orient(source.clone(), Orient::R180);
        assert_eq!((r180.width, r180.height), (3, 2));
        assert_eq!(labels(&r180), vec![6, 5, 4, 3, 2, 1]);

        let r270 = apply_orient(source, Orient::R270);
        assert_eq!((r270.width, r270.height), (2, 3));
        assert_eq!(labels(&r270), vec![3, 6, 2, 5, 1, 4]);
    }

    #[test]
    fn inverse_and_full_turn_rotations_restore_every_pixel() {
        for width in 0..=7 {
            for height in 0..=7 {
                let source = labelled(width, height);
                let restored =
                    apply_orient(apply_orient(source.clone(), Orient::R90), Orient::R270);
                assert_eq!((restored.width, restored.height), (width, height));
                assert_eq!(restored.rgba, source.rgba);

                let mut full_turn = source.clone();
                for _ in 0..4 {
                    full_turn = apply_orient(full_turn, Orient::R90);
                }
                assert_eq!((full_turn.width, full_turn.height), (width, height));
                assert_eq!(full_turn.rgba, source.rgba);
            }
        }
    }

    #[test]
    fn no_op_orientation_and_downscale_preserve_input() {
        let source = labelled(20, 10);
        let oriented = apply_orient(source.clone(), Orient::R0);
        assert_eq!(oriented.rgba, source.rgba);

        let fitted = downscale_to_fit(source.clone(), 20).unwrap();
        assert_eq!((fitted.width, fitted.height), (20, 10));
        assert_eq!(fitted.rgba, source.rgba);
    }

    #[test]
    fn downscale_preserves_aspect_and_storage_invariants() {
        let fitted = downscale_to_fit(labelled(40, 20), 13).unwrap();
        assert_eq!((fitted.width, fitted.height), (13, 7));
        assert_eq!(fitted.rgba.len(), 13 * 7 * 4);

        let portrait = downscale_to_fit(labelled(20, 40), 13).unwrap();
        assert_eq!((portrait.width, portrait.height), (7, 13));
        assert_eq!(portrait.rgba.len(), 7 * 13 * 4);
    }

    #[test]
    fn resize_rejects_malformed_pixel_storage() {
        let malformed = PixelBuf {
            width: 2,
            height: 2,
            rgba: vec![0; 15],
        };
        assert!(resize_exact(malformed, 1, 1).is_err());
    }
}
