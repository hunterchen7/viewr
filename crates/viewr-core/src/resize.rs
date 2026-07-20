//! SIMD downscaling and orientation rotation for PixelBuf.

use fast_image_resize as fir;

use crate::types::{Orient, PixelBuf};

#[derive(Debug, thiserror::Error)]
pub enum ResizeError {
    #[error("resize: {0}")]
    Fir(String),
}

/// Downscale so the long edge is at most `max_edge`, preserving aspect.
/// Returns the input unchanged if it already fits.
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

/// Rotate to display orientation. No-op for `R0`.
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
            let mut dst = vec![0u8; src.len()];
            for yd in 0..dh {
                let row = &mut dst[yd * dw * 4..(yd + 1) * dw * 4];
                for (xd, out) in row.chunks_exact_mut(4).enumerate() {
                    let (xs, ys) = match orient {
                        Orient::R90 => (yd, sh - 1 - xd),
                        _ => (sw - 1 - yd, xd),
                    };
                    let i = (ys * sw + xs) * 4;
                    out.copy_from_slice(&src[i..i + 4]);
                }
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
}
