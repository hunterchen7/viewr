//! RAW → display-ready sRGB pipeline.
//!
//! Same stages as rawler's stock `RawDevelop` (rescale → demosaic → calibrate →
//! crop → sRGB gamma), rebuilt from rawler's public pieces so that:
//! - the demosaic algorithm is selectable: superpixel (half-res, Browse tier)
//!   vs PPG (full-res, Full tier);
//! - the CFA plane is moved, not cloned (stock `develop_intermediate` clones
//!   the whole RawImage);
//! - output is packed RGBA8 directly (no 16-bit DynamicImage detour).

use std::time::{Duration, Instant};

use rawler::RawImage;
use rawler::imgop::matrix::{multiply, normalize, pseudo_inverse};
use rawler::imgop::raw::clip_euclidean_norm_avg;
use rawler::imgop::sensor::bayer::{Demosaic, ppg::PPGDemosaic, superpixel::Superpixel3Channel};
use rawler::imgop::srgb;
use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};
use rawler::pixarray::{Color2D, PixF32};
use rawler::rawimage::{RawImageData, RawPhotometricInterpretation};
use rayon::prelude::*;

use crate::types::PixelBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Superpixel demosaic: each RGGB quad → one RGB pixel, half resolution.
    /// Real raw data, artifact-free, fast. The browse tier.
    Browse,
    /// PPG edge-directed demosaic at native resolution. The 100%-zoom tier.
    Full,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DevelopTimings {
    pub rescale: Duration,
    pub demosaic: Duration,
    pub calibrate: Duration,
    pub gamma_pack: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum DevelopError {
    #[error("rawler: {0}")]
    Rawler(#[from] rawler::RawlerError),
    #[error("unsupported sensor layout: {0}")]
    Unsupported(String),
}

/// Develop a decoded raw into packed sRGB RGBA8.
///
/// Consumes the RawImage (the CFA plane is moved into the pipeline).
/// EXIF orientation is NOT applied here; the caller rotates.
pub fn develop(
    mut raw: RawImage,
    quality: Quality,
) -> Result<(PixelBuf, DevelopTimings), DevelopError> {
    let mut timings = DevelopTimings::default();

    // Black/white level rescale, normalized f32 in place.
    let t = Instant::now();
    raw.apply_scaling()?;
    timings.rescale = t.elapsed();

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return Err(DevelopError::Unsupported("not a CFA sensor".into()));
    };
    if !config.cfa.is_rgb() {
        return Err(DevelopError::Unsupported(format!(
            "non-RGB CFA: {}",
            config.cfa
        )));
    }

    let width = raw.width;
    let height = raw.height;
    // Move the mosaic plane out instead of cloning it (~120MB at 61MP).
    let data = match raw.data {
        RawImageData::Float(v) => v,
        RawImageData::Integer(v) => v.into_iter().map(f32::from).collect(),
    };
    let mosaic = PixF32::new_with(data, width, height);

    // Demosaic over the active (non-masked) sensor area.
    let t = Instant::now();
    let active = raw.active_area.unwrap_or(mosaic.rect());
    let mut image: Color2D<f32, 3> = match quality {
        Quality::Browse => {
            Superpixel3Channel::new().demosaic(&mosaic, &config.cfa, &config.colors, active)
        }
        Quality::Full => PPGDemosaic::new().demosaic(&mosaic, &config.cfa, &config.colors, active),
    };
    drop(mosaic);
    timings.demosaic = t.elapsed();

    // White balance + camera→sRGB(linear), same math as rawler's calibrate step.
    let t = Instant::now();
    let xyz2cam = raw
        .color_matrix
        .iter()
        .find(|(illuminant, _)| **illuminant == Illuminant::D65)
        .or_else(|| raw.color_matrix.iter().next())
        .map(|(_, m)| m.clone());
    if let Some(color_matrix) = xyz2cam.filter(|m| m.len() % 3 == 0) {
        let mut matrix: [[f32; 3]; 4] = [[0.0; 3]; 4];
        for (i, row) in matrix.iter_mut().enumerate().take(color_matrix.len() / 3) {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = color_matrix[i * 3 + j];
            }
        }
        let cam2rgb = pseudo_inverse(normalize(multiply(&matrix, &SRGB_TO_XYZ_D65)));
        let wb = if raw.wb_coeffs[0].is_nan() {
            [1.0; 4]
        } else {
            raw.wb_coeffs
        };
        image.data.par_iter_mut().for_each(|pix| {
            let r = pix[0] * wb[0];
            let g = pix[1] * wb[1];
            let b = pix[2] * wb[2];
            let srgb = [
                cam2rgb[0][0] * r + cam2rgb[0][1] * g + cam2rgb[0][2] * b,
                cam2rgb[1][0] * r + cam2rgb[1][1] * g + cam2rgb[1][2] * b,
                cam2rgb[2][0] * r + cam2rgb[2][1] * g + cam2rgb[2][2] * b,
            ];
            *pix = clip_euclidean_norm_avg(&srgb);
        });
    }
    timings.calibrate = t.elapsed();

    // Crop to the camera-recommended frame (mirrors stock CropDefault logic,
    // including the half-scale adjustment when superpixel halved dimensions).
    if let Some(mut crop) = raw.crop_area.or(raw.active_area) {
        crop = crop.adapt(&raw.active_area.unwrap_or(crop));
        if quality == Quality::Browse {
            crop.scale(0.5);
        }
        if crop.d != image.dim() {
            image = image.crop(crop);
        }
    }

    // sRGB gamma encode + base tone curve + pack to RGBA8.
    let t = Instant::now();
    let out_w = image.width as u32;
    let out_h = image.height as u32;
    let mut rgba = vec![0u8; image.data.len() * 4];
    image
        .data
        .par_iter()
        .zip(rgba.par_chunks_exact_mut(4))
        .for_each(|(pix, out)| {
            let p = srgb::srgb_apply_gamma_n(*pix);
            out[0] = (base_curve(p[0].clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
            out[1] = (base_curve(p[1].clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
            out[2] = (base_curve(p[2].clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
            out[3] = 255;
        });
    timings.gamma_pack = t.elapsed();

    Ok((
        PixelBuf {
            width: out_w,
            height: out_h,
            rgba,
        },
        timings,
    ))
}

pub fn total(timings: &DevelopTimings) -> Duration {
    timings.rescale + timings.demosaic + timings.calibrate + timings.gamma_pack
}

/// Fixed "camera-standard-ish" base tone curve: a mild S in display
/// space (smoothstep blend) so renders match the punch of camera JPEGs
/// instead of looking flat. No editing UI — it's a culler; the constant
/// is versioned by `cache_disk::DEVELOP_VERSION`.
const TONE_STRENGTH: f32 = 0.35;

#[inline]
fn base_curve(v: f32) -> f32 {
    let s = v * v * (3.0 - 2.0 * v); // smoothstep
    v + TONE_STRENGTH * (s - v)
}

#[cfg(test)]
mod tests {
    use super::{DevelopTimings, base_curve, total};
    use std::time::Duration;

    #[test]
    fn base_curve_is_monotonic_and_anchored() {
        assert_eq!(base_curve(0.0), 0.0);
        assert!((base_curve(1.0) - 1.0).abs() < 1e-6);
        assert!((base_curve(0.5) - 0.5).abs() < 1e-6); // midpoint fixed
        assert!(base_curve(0.25) < 0.25); // shadows deepen
        assert!(base_curve(0.75) > 0.75); // highlights lift
        let mut prev = 0.0;
        for i in 1..=100 {
            let v = base_curve(i as f32 / 100.0);
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn total_includes_every_reported_develop_stage() {
        let timings = DevelopTimings {
            rescale: Duration::from_millis(1),
            demosaic: Duration::from_millis(2),
            calibrate: Duration::from_millis(3),
            gamma_pack: Duration::from_millis(4),
        };
        assert_eq!(total(&timings), Duration::from_millis(10));
    }

    #[test]
    fn base_curve_stays_in_display_range_for_dense_samples() {
        for step in 0..=10_000 {
            let input = step as f32 / 10_000.0;
            let output = base_curve(input);
            assert!((0.0..=1.0).contains(&output));
        }
    }
}
