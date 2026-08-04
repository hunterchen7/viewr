//! RAW → gamma/tone-packed RGBA8 pipeline.
//!
//! Same stages as rawler's stock `RawDevelop` (rescale → demosaic → calibrate →
//! crop → sRGB gamma), rebuilt from rawler's public pieces so that:
//! - the demosaic algorithm is selectable: superpixel (half-res, Browse tier)
//!   vs PPG (full-res, Full tier);
//! - the CFA plane is moved, not cloned (stock `develop_intermediate` clones
//!   the whole RawImage);
//! - output is packed RGBA8 directly (no 16-bit DynamicImage detour).
//!
//! A usable camera color matrix produces display-sRGB samples. As in rawler's
//! fallback, a missing or unusable matrix leaves samples in camera space before
//! gamma/tone packing.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rawler::cfa::PlaneColor;
use rawler::imgop::matrix::{multiply, normalize, pseudo_inverse};
use rawler::imgop::raw::{clip_euclidean_norm_avg, correct_blacklevel_cfa};
use rawler::imgop::sensor::bayer::ppg::RegionDemosaic;
use rawler::imgop::sensor::bayer::{Demosaic, ppg::PPGDemosaic, superpixel::Superpixel3Channel};
use rawler::imgop::srgb;
use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};
use rawler::imgop::{Dim2, Point, Rect};
use rawler::pixarray::{Color2D, PixF32};
use rawler::rawimage::{RawImageData, RawPhotometricInterpretation};
use rawler::{CFA, RawImage};
use rayon::prelude::*;

use crate::types::{Orient, PixelBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Demosaic quality and output-size tier.
pub enum Quality {
    /// Superpixel demosaic: each 2×2 Bayer quad → one RGB pixel, half resolution.
    /// Uses real RAW samples without interpolating neighboring quads. The
    /// browse tier.
    Browse,
    /// PPG edge-directed demosaic at native resolution. The 100%-zoom tier.
    Full,
}

#[derive(Debug, Default, Clone, Copy)]
/// Wall-clock time spent in each stage of one RAW development.
///
/// These fields exclude file decode, EXIF orientation, caching, and JPEG
/// encoding. Parallel stages are reported as elapsed wall time, not summed CPU
/// time.
pub struct DevelopTimings {
    /// Black/white-level normalization of the CFA mosaic.
    pub rescale: Duration,
    /// CFA-to-RGB demosaic.
    pub demosaic: Duration,
    /// Optional white balance and camera-to-linear-sRGB color conversion.
    pub calibrate: Duration,
    /// Gamma, tone curve, and RGBA8 packing.
    pub gamma_pack: Duration,
}

#[derive(Debug, thiserror::Error)]
/// Failure while converting a decoded CFA mosaic into display pixels.
pub enum DevelopError {
    /// A `rawler` image operation failed.
    #[error("rawler: {0}")]
    Rawler(#[from] rawler::RawlerError),
    /// The sensor photometric interpretation or CFA layout is unsupported.
    #[error("unsupported sensor layout: {0}")]
    Unsupported(String),
}

fn usable_color_matrix(matrix: &[f32]) -> bool {
    matches!(matrix.len(), 9 | 12) && matrix.iter().all(|value| value.is_finite())
}

fn sanitize_white_balance(coefficients: [f32; 4]) -> [f32; 4] {
    if coefficients[..3]
        .iter()
        .all(|value| value.is_finite() && *value > 0.0)
    {
        coefficients
    } else {
        [1.0; 4]
    }
}

/// Develop a decoded raw into packed RGBA8.
///
/// Consumes the RawImage (the CFA plane is moved into the pipeline).
/// EXIF orientation is NOT applied here; the caller rotates.
/// The returned [`PixelBuf`] is tightly packed RGBA8. A usable camera color
/// matrix produces display sRGB; when that matrix is missing or unusable, the
/// rawler-compatible fallback preserves camera-space samples before gamma and
/// tone packing.
///
/// # Errors
///
/// Returns [`DevelopError::Unsupported`] for non-CFA or non-RGB CFA sensors and
/// [`DevelopError::Rawler`] when an upstream RAW operation fails.
pub fn develop(
    raw: RawImage,
    quality: Quality,
) -> Result<(PixelBuf, DevelopTimings), DevelopError> {
    develop_with_gamma(raw, quality, GammaMode::Lut)
}

#[derive(Clone, Copy)]
enum GammaMode {
    Lut,
    #[cfg(test)]
    Analytical,
}

fn develop_with_gamma(
    raw: RawImage,
    quality: Quality,
    gamma_mode: GammaMode,
) -> Result<(PixelBuf, DevelopTimings), DevelopError> {
    develop_with_layout(raw, quality, gamma_mode, RegionLayout::Strided)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegionLayout {
    Strided,
    #[cfg(test)]
    Copied,
}

fn develop_with_layout(
    raw: RawImage,
    quality: Quality,
    gamma_mode: GammaMode,
    region_layout: RegionLayout,
) -> Result<(PixelBuf, DevelopTimings), DevelopError> {
    let mut timings = DevelopTimings::default();

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return Err(DevelopError::Unsupported("not a CFA sensor".into()));
    };
    if !config.cfa.is_rgb() {
        return Err(DevelopError::Unsupported(format!(
            "non-RGB CFA: {}",
            config.cfa
        )));
    }

    let calibration = derive_calibration(&raw);

    // Convert and normalize integer mosaics in one traversal. rawler's
    // RawImage::apply_scaling first writes a complete f32 copy and then
    // revisits it for black/white correction; development consumes the raw,
    // so the intermediate unscaled f32 frame is unnecessary.
    let t = Instant::now();
    let width = raw.width;
    let height = raw.height;
    let blacklevel = raw.blacklevel.as_bayer_array();
    let whitelevel = raw.whitelevel.as_bayer_array();
    // Move the mosaic plane out instead of cloning it (~120MB at 61MP).
    let data = scale_cfa_data(raw.data, width, height, blacklevel, whitelevel);
    let mosaic = PixF32::new_with(data, width, height);
    timings.rescale = t.elapsed();

    // Demosaic over the active (non-masked) sensor area.
    let t = Instant::now();
    let active = raw.active_area.unwrap_or(mosaic.rect());
    let mut image: Color2D<f32, 3> = match quality {
        Quality::Browse => superpixel_demosaic(&mosaic, &config.cfa, &config.colors, active),
        Quality::Full => PPGDemosaic::new().demosaic(&mosaic, &config.cfa, &config.colors, active),
    };
    drop(mosaic);
    timings.demosaic = t.elapsed();

    // Keep the demosaiced frame in place. Calibration and packing operate only
    // on this region, avoiding Color2D::crop's second full output allocation
    // and copy while preserving its row-major output order.
    let region = develop_region(image.dim(), raw.crop_area, raw.active_area, quality);
    let region = match region_layout {
        RegionLayout::Strided => region,
        #[cfg(test)]
        RegionLayout::Copied if region.d != image.dim() => {
            image = image.crop(region);
            image.rect()
        }
        #[cfg(test)]
        RegionLayout::Copied => region,
    };

    // White balance + camera→sRGB(linear), same math as rawler's calibrate step.
    let t = Instant::now();
    if let Some((cam2rgb, wb)) = calibration {
        for_each_region_pixel_mut(&mut image, region, |pix| {
            calibrate_pixel(&cam2rgb, &wb, pix);
        });
    }
    timings.calibrate = t.elapsed();

    // sRGB gamma encode + base tone curve + pack to RGBA8.
    let t = Instant::now();
    let out_w = region.width() as u32;
    let out_h = region.height() as u32;
    let mut rgba = vec![0u8; region.width() * region.height() * 4];
    match gamma_mode {
        GammaMode::Lut => {
            let table = gamma_tone_lut();
            pack_region_rgba(&image, region, &mut rgba, |value| {
                gamma_tone_pack(value, table)
            });
        }
        #[cfg(test)]
        GammaMode::Analytical => {
            pack_region_rgba(&image, region, &mut rgba, analytical_gamma_tone_pack)
        }
    }
    timings.gamma_pack = t.elapsed();

    Ok((PixelBuf::new_opaque(out_w, out_h, rgba), timings))
}

fn scale_cfa_data(
    data: RawImageData,
    width: usize,
    height: usize,
    blacklevel: [f32; 4],
    whitelevel: [f32; 4],
) -> Vec<f32> {
    let data = match data {
        RawImageData::Float(mut data) => {
            correct_blacklevel_cfa(&mut data, width, height, &blacklevel, &whitelevel);
            return data;
        }
        RawImageData::Integer(data) => data,
    };
    assert_eq!(data.len(), width * height);
    let output_len = data.len();
    if output_len == 0 {
        return Vec::new();
    }

    let maximum = [
        whitelevel[0] - blacklevel[0],
        whitelevel[1] - blacklevel[1],
        whitelevel[2] - blacklevel[2],
        whitelevel[3] - blacklevel[3],
    ];
    let scale = |value: u16, channel: usize| {
        let value = f32::from(value) - blacklevel[channel];
        (if value.is_sign_negative() { 0.0 } else { value }) / maximum[channel]
    };

    let mut output = Vec::<f32>::with_capacity(output_len);
    {
        let spare = &mut output.spare_capacity_mut()[..output_len];
        let row_pair_samples = width * 2;
        let paired_samples = data.len() / row_pair_samples * row_pair_samples;
        let fill_pair = |input: &[u16], output: &mut [std::mem::MaybeUninit<f32>]| {
            let (input_top, input_bottom) = input.split_at(width);
            let (output_top, output_bottom) = output.split_at_mut(width);
            for (input, output) in input_top
                .chunks_exact(2)
                .zip(output_top.chunks_exact_mut(2))
            {
                output[0].write(scale(input[0], 0));
                output[1].write(scale(input[1], 1));
            }
            for (input, output) in input_bottom
                .chunks_exact(2)
                .zip(output_bottom.chunks_exact_mut(2))
            {
                output[0].write(scale(input[0], 2));
                output[1].write(scale(input[1], 3));
            }
            if !width.is_multiple_of(2) {
                output_top[width - 1].write(f32::from(input_top[width - 1]));
                output_bottom[width - 1].write(f32::from(input_bottom[width - 1]));
            }
        };
        #[cfg(not(miri))]
        data[..paired_samples]
            .par_chunks_exact(row_pair_samples)
            .zip(spare[..paired_samples].par_chunks_exact_mut(row_pair_samples))
            .for_each(|(input, output)| fill_pair(input, output));
        #[cfg(miri)]
        data[..paired_samples]
            .chunks_exact(row_pair_samples)
            .zip(spare[..paired_samples].chunks_exact_mut(row_pair_samples))
            .for_each(|(input, output)| fill_pair(input, output));
        data[paired_samples..]
            .iter()
            .zip(&mut spare[paired_samples..])
            .for_each(|(input, output)| {
                output.write(f32::from(*input));
            });
    }
    // SAFETY: every output slot is initialized exactly once above. Full row
    // pairs receive corrected samples (plus rawler-compatible odd-width
    // tails), and a possible final odd row receives unscaled f32 samples to
    // match correct_blacklevel_cfa's chunks_exact behavior.
    unsafe { output.set_len(output_len) };
    output
}

#[cfg(feature = "benchmarks")]
#[doc(hidden)]
/// Runs the production fused integer conversion and CFA normalization.
pub fn benchmark_scale_cfa_fused(raw: RawImage) -> Vec<f32> {
    scale_cfa_data(
        raw.data,
        raw.width,
        raw.height,
        raw.blacklevel.as_bayer_array(),
        raw.whitelevel.as_bayer_array(),
    )
}

#[cfg(feature = "benchmarks")]
#[doc(hidden)]
/// Runs rawler's former two-pass normalization as a benchmark reference.
pub fn benchmark_scale_cfa_legacy(mut raw: RawImage) -> Vec<f32> {
    raw.apply_scaling()
        .expect("the decoded benchmark RAW scales");
    match raw.data {
        RawImageData::Float(data) => data,
        RawImageData::Integer(_) => unreachable!("apply_scaling always produces f32 samples"),
    }
}

/// Derives the white-balance and camera→sRGB calibration used by the
/// per-pixel calibrate step, with the same finite/usable guards as rawler's
/// fallback behavior. `None` leaves samples in camera space.
fn derive_calibration(raw: &RawImage) -> Option<([[f32; 4]; 3], [f32; 4])> {
    let color_matrix = raw
        .color_matrix
        .iter()
        .find(|(illuminant, _)| **illuminant == Illuminant::D65)
        .or_else(|| raw.color_matrix.iter().next())
        .map(|(_, m)| m.clone())
        .filter(|matrix| usable_color_matrix(matrix))?;
    let mut matrix: [[f32; 3]; 4] = [[0.0; 3]; 4];
    for (i, row) in matrix.iter_mut().enumerate().take(color_matrix.len() / 3) {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = color_matrix[i * 3 + j];
        }
    }
    let cam2rgb = pseudo_inverse(normalize(multiply(&matrix, &SRGB_TO_XYZ_D65)));
    if !cam2rgb.iter().flatten().all(|value| value.is_finite()) {
        return None;
    }
    Some((cam2rgb, sanitize_white_balance(raw.wb_coeffs)))
}

/// White balance + camera→sRGB(linear) for one pixel, identical to the
/// closure historically inlined in [`develop_with_layout`].
#[inline]
fn calibrate_pixel(cam2rgb: &[[f32; 4]; 3], wb: &[f32; 4], pix: &mut [f32; 3]) {
    let r = pix[0] * wb[0];
    let g = pix[1] * wb[1];
    let b = pix[2] * wb[2];
    let srgb = [
        cam2rgb[0][0] * r + cam2rgb[0][1] * g + cam2rgb[0][2] * b,
        cam2rgb[1][0] * r + cam2rgb[1][1] * g + cam2rgb[1][2] * b,
        cam2rgb[2][0] * r + cam2rgb[2][1] * g + cam2rgb[2][2] * b,
    ];
    *pix = clip_euclidean_norm_avg(&srgb);
}

/// Whether a decoded raw can be developed region by region.
///
/// Mirrors [`develop`]'s validity checks plus the ≥6×6 active-area floor of
/// the fork's `demosaic_region`. Callers fall back to the monolithic path when
/// this returns `false`.
pub fn supports_region_develop(raw: &RawImage) -> bool {
    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return false;
    };
    if !config.cfa.is_rgb() {
        return false;
    }
    let dim = Dim2::new(raw.width, raw.height);
    let active = raw.active_area.unwrap_or(Rect::new(Point::zero(), dim));
    active.d.w >= 6
        && active.d.h >= 6
        && active.p.x + active.d.w <= dim.w
        && active.p.y + active.d.h <= dim.h
}

/// Reusable state for developing one Full-tier frame region by region.
///
/// Built once per frame by [`plan_full_develop`]; each
/// [`develop_region_into`](Self::develop_region_into) call then demosaics,
/// calibrates, and gamma/tone-packs one output rectangle directly into an
/// oriented display-space RGBA8 canvas. Assembling every region reproduces
/// `apply_orient(develop(raw, Full), orient)` byte for byte.
pub struct FullDevelopPlan {
    /// Black/white-normalized CFA mosaic ([`scale_cfa_data`] output).
    mosaic: PixF32,
    cfa: CFA,
    colors: PlaneColor,
    /// Demosaic roi (the active sensor area).
    active: Rect,
    /// Output crop in demosaic-output coordinates
    /// ([`develop_region`] for the Full tier).
    out_region: Rect,
    /// Camera→sRGB matrix and sanitized white balance, when usable.
    calibration: Option<([[f32; 4]; 3], [f32; 4])>,
}

/// Prepares a [`FullDevelopPlan`] from a decoded raw.
///
/// Runs the same validity checks and mosaic normalization as
/// [`develop`] with [`Quality::Full`], but stops before demosaic so regions
/// can be developed on demand.
///
/// # Errors
///
/// Returns [`DevelopError::Unsupported`] for non-CFA or non-RGB CFA sensors.
pub fn plan_full_develop(raw: RawImage) -> Result<FullDevelopPlan, DevelopError> {
    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return Err(DevelopError::Unsupported("not a CFA sensor".into()));
    };
    if !config.cfa.is_rgb() {
        return Err(DevelopError::Unsupported(format!(
            "non-RGB CFA: {}",
            config.cfa
        )));
    }
    let cfa = config.cfa.clone();
    let colors = config.colors.clone();
    let calibration = derive_calibration(&raw);

    let width = raw.width;
    let height = raw.height;
    let blacklevel = raw.blacklevel.as_bayer_array();
    let whitelevel = raw.whitelevel.as_bayer_array();
    let crop_area = raw.crop_area;
    let active_area = raw.active_area;
    let data = scale_cfa_data(raw.data, width, height, blacklevel, whitelevel);
    let mosaic = PixF32::new_with(data, width, height);

    let active = active_area.unwrap_or(mosaic.rect());
    let out_region = develop_region(active.d, crop_area, active_area, Quality::Full);
    Ok(FullDevelopPlan {
        mosaic,
        cfa,
        colors,
        active,
        out_region,
        calibration,
    })
}

impl FullDevelopPlan {
    /// Output dimensions before display orientation, matching
    /// `develop(raw, Quality::Full)`.
    pub fn output_size(&self) -> (u32, u32) {
        (self.out_region.d.w as u32, self.out_region.d.h as u32)
    }

    /// Output dimensions after display orientation, matching
    /// `apply_orient(develop(raw, Quality::Full), orient)`.
    pub fn display_size(&self, orient: Orient) -> (u32, u32) {
        let (w, h) = self.output_size();
        if orient.swaps_axes() { (h, w) } else { (w, h) }
    }

    /// Maps a pre-orient output rectangle to its display-space rectangle as
    /// `[x, y, width, height]`.
    pub fn display_rect(&self, out_rect: Rect, orient: Orient) -> [u32; 4] {
        oriented_display_rect(out_rect, self.out_region.d, orient)
    }

    /// Develops `out_rect` (pre-orient output coordinates), calibrates it, and
    /// writes gamma/tone-packed oriented RGBA8 into `canvas`.
    ///
    /// `canvas` is a row-major RGBA8 buffer with `canvas_w` pixels per row
    /// whose top-left pixel sits at display coordinate `canvas_origin`; a
    /// whole-frame canvas passes `[0, 0]` with `canvas_w == display_size().0`,
    /// while a scratch buffer for just this region passes the rectangle
    /// returned by [`display_rect`](Self::display_rect). Returns the display
    /// rectangle written, `[x, y, width, height]`.
    ///
    /// Each display pixel receives exactly the bytes that packing the
    /// developed pixel and then permuting through
    /// [`crate::resize::apply_orient`] would place there, so any disjoint or
    /// overlapping cover of the output frame assembles the identical
    /// full-frame image.
    ///
    /// # Panics
    ///
    /// Panics when `out_rect` escapes [`output_size`](Self::output_size) or
    /// the mapped display rectangle does not fit inside the canvas.
    pub fn develop_region_into(
        &self,
        out_rect: Rect,
        orient: Orient,
        canvas: &mut [u8],
        canvas_w: u32,
        canvas_origin: [u32; 2],
    ) -> [u32; 4] {
        assert!(out_rect.p.x + out_rect.d.w <= self.out_region.d.w);
        assert!(out_rect.p.y + out_rect.d.h <= self.out_region.d.h);

        // Demosaic exactly the halo-padded region (active-local coordinates).
        let dm_region = Rect::new(
            Point::new(
                self.out_region.p.x + out_rect.p.x,
                self.out_region.p.y + out_rect.p.y,
            ),
            out_rect.d,
        );
        let RegionDemosaic { mut rgb, valid } = PPGDemosaic::new().demosaic_region(
            &self.mosaic,
            &self.cfa,
            &self.colors,
            self.active,
            dm_region,
        );

        if let Some((cam2rgb, wb)) = self.calibration {
            for_each_region_pixel_mut(&mut rgb, valid, |pix| {
                calibrate_pixel(&cam2rgb, &wb, pix);
            });
        }

        let table = gamma_tone_lut();
        pack_oriented_region(
            &rgb,
            valid,
            out_rect,
            self.out_region.d,
            orient,
            canvas,
            canvas_w,
            canvas_origin,
            |value| gamma_tone_pack(value, table),
        )
    }
}

/// Display-space rectangle of `out_rect` within a `out_dim` frame under
/// `orient`, as `[x, y, width, height]`.
///
/// Uses the forward form of [`crate::resize::apply_orient`]'s inverse pixel
/// mappings (R90 reads source `(yd, sh-1-xd)`, R270 reads `(sw-1-yd, xd)`).
fn oriented_display_rect(out_rect: Rect, out_dim: Dim2, orient: Orient) -> [u32; 4] {
    let (x, y, w, h) = (out_rect.p.x, out_rect.p.y, out_rect.d.w, out_rect.d.h);
    let (out_w, out_h) = (out_dim.w, out_dim.h);
    let [dx, dy, dw, dh] = match orient {
        Orient::R0 => [x, y, w, h],
        Orient::R90 => [out_h - y - h, x, h, w],
        Orient::R180 => [out_w - x - w, out_h - y - h, w, h],
        Orient::R270 => [y, out_w - x - w, h, w],
    };
    [dx as u32, dy as u32, dw as u32, dh as u32]
}

/// Packs the `out_rect` portion of a strided region buffer directly into its
/// oriented display-space position: a fusion of [`pack_region_rgba`] with the
/// pixel permutation of [`crate::resize::apply_orient`].
///
/// `rgb` holds the developed pixels with `out_rect`'s top-left at
/// `valid.p`; `canvas` is described in
/// [`FullDevelopPlan::develop_region_into`]. Returns the display rectangle
/// written.
#[allow(clippy::too_many_arguments)]
fn pack_oriented_region<F>(
    rgb: &Color2D<f32, 3>,
    valid: Rect,
    out_rect: Rect,
    out_dim: Dim2,
    orient: Orient,
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_origin: [u32; 2],
    pack_channel: F,
) -> [u32; 4]
where
    F: Fn(f32) -> u8 + Sync,
{
    assert!(valid.p.x + out_rect.d.w <= rgb.width);
    assert!(valid.p.y + out_rect.d.h <= rgb.height);
    let display = oriented_display_rect(out_rect, out_dim, orient);
    let [dx, dy, dw, dh] = display.map(|value| value as usize);
    let canvas_w = canvas_w as usize;
    let origin_x = canvas_origin[0] as usize;
    let origin_y = canvas_origin[1] as usize;
    assert!(origin_x <= dx && dx + dw <= origin_x + canvas_w);
    assert!(origin_y <= dy);
    let stride = canvas_w * 4;
    let row_first = dy - origin_y;
    let rows = &mut canvas[row_first * stride..(row_first + dh) * stride];
    let (out_w, out_h) = (out_dim.w, out_dim.h);

    rows.par_chunks_exact_mut(stride)
        .enumerate()
        .for_each(|(row_offset, row)| {
            let yd = dy + row_offset;
            for xd in dx..dx + dw {
                let (xs, ys) = match orient {
                    Orient::R0 => (xd, yd),
                    Orient::R90 => (yd, out_h - 1 - xd),
                    Orient::R180 => (out_w - 1 - xd, out_h - 1 - yd),
                    Orient::R270 => (out_w - 1 - yd, xd),
                };
                let pix = rgb.at(
                    valid.p.y + (ys - out_rect.p.y),
                    valid.p.x + (xs - out_rect.p.x),
                );
                let base = (xd - origin_x) * 4;
                let out = &mut row[base..base + 4];
                out[0] = pack_channel(pix[0]);
                out[1] = pack_channel(pix[1]);
                out[2] = pack_channel(pix[2]);
                out[3] = 255;
            }
        });
    display
}

fn develop_region(
    image_dim: Dim2,
    crop_area: Option<Rect>,
    active_area: Option<Rect>,
    quality: Quality,
) -> Rect {
    if let Some(mut crop) = crop_area.or(active_area) {
        crop = crop.adapt(&active_area.unwrap_or(crop));
        if quality == Quality::Browse {
            crop.scale(0.5);
        }
        if crop.d != image_dim {
            return crop;
        }
    }
    Rect::new(Point::zero(), image_dim)
}

fn for_each_region_pixel_mut<F>(image: &mut Color2D<f32, 3>, region: Rect, transform: F)
where
    F: Fn(&mut [f32; 3]) + Sync,
{
    assert_region_within(image, region);
    if region.is_empty() {
        return;
    }

    let first = region.y() * image.width;
    let last = (region.y() + region.height()) * image.width;
    image.data[first..last]
        .par_chunks_exact_mut(image.width)
        .for_each(|row| {
            row[region.x()..region.x() + region.width()]
                .iter_mut()
                .for_each(&transform);
        });
}

fn pack_region_rgba<F>(image: &Color2D<f32, 3>, region: Rect, rgba: &mut [u8], pack_channel: F)
where
    F: Fn(f32) -> u8 + Sync,
{
    assert_region_within(image, region);
    assert_eq!(rgba.len(), region.width() * region.height() * 4);
    if region.is_empty() {
        return;
    }

    let first = region.y() * image.width;
    let last = (region.y() + region.height()) * image.width;
    image.data[first..last]
        .par_chunks_exact(image.width)
        .zip(rgba.par_chunks_exact_mut(region.width() * 4))
        .for_each(|(row, out_row)| {
            row[region.x()..region.x() + region.width()]
                .iter()
                .zip(out_row.chunks_exact_mut(4))
                .for_each(|(pix, out)| {
                    out[0] = pack_channel(pix[0]);
                    out[1] = pack_channel(pix[1]);
                    out[2] = pack_channel(pix[2]);
                    out[3] = 255;
                });
        });
}

fn assert_region_within(image: &Color2D<f32, 3>, region: Rect) {
    assert!(region.y() + region.height() <= image.height);
    assert!(region.x() + region.width() <= image.width);
}

/// Sums the stages measured by [`develop`].
///
/// This deliberately excludes RAW file decode, display rotation, cache work,
/// and JPEG encoding.
pub fn total(timings: &DevelopTimings) -> Duration {
    timings.rescale + timings.demosaic + timings.calibrate + timings.gamma_pack
}

/// Equivalent to rawler's three-channel superpixel demosaic, but writes each
/// output pixel directly into its final allocation. Rawler currently builds a
/// temporary `Vec` for every output row and then copies all rows into another
/// `Vec`; that extra allocation and full-frame copy dominate Browse demosaic on
/// large sensors.
fn superpixel_demosaic(
    mosaic: &PixF32,
    cfa: &CFA,
    colors: &PlaneColor,
    roi: Rect,
) -> Color2D<f32, 3> {
    let out_width = (roi.width() & !1) / 2;
    let out_height = (roi.height() & !1) / 2;
    let out_len = out_width * out_height;
    if out_len == 0 {
        return Color2D::new_with(Vec::new(), out_width, out_height);
    }

    // A two-pixel step keeps the shifted 2x2 Bayer pattern constant across
    // the output, so resolve the channel positions once outside the hot loop.
    let shifted = cfa.shift(roi.x(), roi.y());
    let [red, green_a, green_b, blue] = match shifted.name.as_str() {
        "RGGB" => [0, 1, 2, 3],
        "BGGR" => [3, 1, 2, 0],
        "GBRG" => [2, 0, 3, 1],
        "GRBG" => [1, 0, 3, 2],
        _ => return Superpixel3Channel::new().demosaic(mosaic, cfa, colors, roi),
    };

    let mut output = Vec::<[f32; 3]>::with_capacity(out_len);
    {
        let spare = &mut output.spare_capacity_mut()[..out_len];
        let fill_row = |out_y: usize, out_row: &mut [std::mem::MaybeUninit<[f32; 3]>]| {
            let top_start = (roi.y() + out_y * 2) * mosaic.width + roi.x();
            let bottom_start = top_start + mosaic.width;
            let top = &mosaic.data[top_start..top_start + out_width * 2];
            let bottom = &mosaic.data[bottom_start..bottom_start + out_width * 2];

            for (out, (top_pair, bottom_pair)) in out_row
                .iter_mut()
                .zip(top.chunks_exact(2).zip(bottom.chunks_exact(2)))
            {
                let values = [top_pair[0], top_pair[1], bottom_pair[0], bottom_pair[1]];
                out.write([
                    values[red],
                    (values[green_a] + values[green_b]) / 2.0,
                    values[blue],
                ]);
            }
        };
        #[cfg(not(miri))]
        spare
            .par_chunks_exact_mut(out_width)
            .enumerate()
            .for_each(|(out_y, out_row)| fill_row(out_y, out_row));
        // Miri cannot execute Rayon's Crossbeam deque under its pointer model.
        // A serial traversal validates the identical initialization contract.
        #[cfg(miri)]
        spare
            .chunks_exact_mut(out_width)
            .enumerate()
            .for_each(|(out_y, out_row)| fill_row(out_y, out_row));
    }

    // SAFETY: `spare` covers exactly `out_len` slots and the row traversal
    // initializes each slot once before the vector length becomes observable.
    unsafe { output.set_len(out_len) };
    Color2D::new_with(output, out_width, out_height)
}

/// Fixed "camera-standard-ish" rendering: a highlight-preserving exposure
/// lift followed by a mild display-space S curve. RAW values otherwise look
/// darker and flatter than the camera JPEG used for normal culling. There is
/// no editing UI; these constants are versioned by
/// `cache_disk::DEVELOP_VERSION`.
const CULLING_EXPOSURE_GAIN: f32 = 1.3;
const TONE_STRENGTH: f32 = 0.35;

// Development ends in an 8-bit buffer, so evaluating three `powf` calls for
// every output pixel cannot preserve any information beyond the final 256
// values. A 16-bit linear-light lookup domain keeps the maximum difference
// from the analytical transfer function to one output level while replacing
// those transcendental calls with cache-resident table reads. The table is
// shared by every develop and initialized only on first use.
const GAMMA_LUT_LEN: usize = 1 << 16;
static GAMMA_TONE_LUT: OnceLock<Box<[u8; GAMMA_LUT_LEN]>> = OnceLock::new();

#[inline]
fn analytical_gamma_tone_pack(v: f32) -> u8 {
    let gamma = srgb::srgb_apply_gamma(exposure_rolloff(v));
    (base_curve(gamma.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8
}

/// Precomputes the shared gamma/tone lookup table.
///
/// The table is otherwise built lazily inside the first develop's timed gamma
/// stage; warming it during application startup moves those transcendental
/// evaluations off the first image's critical path. Safe to call from any
/// thread, any number of times.
pub fn warm_gamma_lut() {
    let _ = gamma_tone_lut();
}

fn gamma_tone_lut() -> &'static [u8; GAMMA_LUT_LEN] {
    GAMMA_TONE_LUT.get_or_init(|| {
        let mut table = Box::new([0; GAMMA_LUT_LEN]);
        for (index, output) in table.iter_mut().enumerate() {
            *output = analytical_gamma_tone_pack(index as f32 / (GAMMA_LUT_LEN - 1) as f32);
        }
        table
    })
}

#[inline]
fn gamma_tone_pack(v: f32, table: &[u8; GAMMA_LUT_LEN]) -> u8 {
    let index = (v.clamp(0.0, 1.0) * (GAMMA_LUT_LEN - 1) as f32 + 0.5) as usize;
    table[index]
}

#[inline]
fn exposure_rolloff(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    CULLING_EXPOSURE_GAIN * v / (1.0 + (CULLING_EXPOSURE_GAIN - 1.0) * v)
}

#[inline]
fn base_curve(v: f32) -> f32 {
    let s = v * v * (3.0 - 2.0 * v); // smoothstep
    v + TONE_STRENGTH * (s - v)
}

#[cfg(test)]
mod tests {
    use super::{
        DevelopTimings, analytical_gamma_tone_pack, base_curve, exposure_rolloff, gamma_tone_pack,
        sanitize_white_balance, total, usable_color_matrix,
    };
    use rawler::CFA;
    use rawler::cfa::PlaneColor;
    use rawler::imgop::sensor::bayer::{Demosaic, superpixel::Superpixel3Channel};
    use rawler::imgop::{Dim2, Point, Rect};
    use rawler::pixarray::PixF32;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn real_sony_raw_fixture() -> PathBuf {
        std::env::var_os("VIEWR_TEST_RAW")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../testdata/real-raw-corpus/HCA04875.ARW")
            })
    }

    #[test]
    fn malformed_color_calibration_values_fail_closed() {
        assert!(usable_color_matrix(&[1.0; 9]));
        assert!(usable_color_matrix(&[1.0; 12]));
        assert!(!usable_color_matrix(&[1.0; 6]));
        let mut non_finite_matrix = [1.0; 9];
        non_finite_matrix[4] = f32::NAN;
        assert!(!usable_color_matrix(&non_finite_matrix));

        let valid = sanitize_white_balance([2.0, 1.0, 1.5, f32::NAN]);
        assert_eq!(&valid[..3], &[2.0, 1.0, 1.5]);
        assert!(valid[3].is_nan());
        for invalid in [
            [f32::NAN, 1.0, 1.0, 1.0],
            [1.0, f32::INFINITY, 1.0, 1.0],
            [1.0, 1.0, 0.0, 1.0],
        ] {
            assert_eq!(sanitize_white_balance(invalid), [1.0; 4]);
        }
    }

    #[test]
    fn fused_integer_cfa_scaling_matches_rawler_exactly() {
        let blacklevel = [64.0, 72.0, 80.0, 96.0];
        let whitelevel = [4_095.0, 4_000.0, 3_900.0, 3_800.0];

        for (width, height) in [(8, 6), (7, 6), (8, 5), (7, 5)] {
            let integer: Vec<u16> = (0..width * height)
                .map(|index| ((index * 137 + 17) % 4_096) as u16)
                .collect();
            let mut expected: Vec<f32> = integer.iter().copied().map(f32::from).collect();
            #[cfg(not(miri))]
            rawler::imgop::raw::correct_blacklevel_cfa(
                &mut expected,
                width,
                height,
                &blacklevel,
                &whitelevel,
            );
            #[cfg(miri)]
            for lines in expected.chunks_exact_mut(width * 2) {
                for (index, value) in lines.iter_mut().enumerate() {
                    let channel = usize::from(index >= width) * 2 + index % width % 2;
                    if !width.is_multiple_of(2) && index % width == width - 1 {
                        continue;
                    }
                    let corrected = *value - blacklevel[channel];
                    *value = (if corrected.is_sign_negative() {
                        0.0
                    } else {
                        corrected
                    }) / (whitelevel[channel] - blacklevel[channel]);
                }
            }
            let actual = super::scale_cfa_data(
                rawler::rawimage::RawImageData::Integer(integer),
                width,
                height,
                blacklevel,
                whitelevel,
            );
            assert_eq!(actual, expected, "{width}x{height}");
        }
    }

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
    fn exposure_lift_brightens_midtones_and_preserves_highlights() {
        assert_eq!(exposure_rolloff(0.0), 0.0);
        assert_eq!(exposure_rolloff(1.0), 1.0);

        let middle_gray = exposure_rolloff(0.18);
        assert!(middle_gray > 0.18);
        assert!(middle_gray < 0.18 * super::CULLING_EXPOSURE_GAIN);
        let effective_ev = (middle_gray / 0.18).log2();
        assert!(
            (0.3..=0.4).contains(&effective_ev),
            "unexpected middle-gray lift: {effective_ev} EV"
        );

        let highlight = exposure_rolloff(0.9);
        assert!((0.9..1.0).contains(&highlight));

        let mut previous = 0.0;
        for step in 1..=10_000 {
            let output = exposure_rolloff(step as f32 / 10_000.0);
            assert!(output >= previous);
            previous = output;
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

    #[test]
    fn gamma_tone_lut_is_within_one_output_level_of_analytical_curve() {
        let table = super::gamma_tone_lut();
        for step in 0..=200_000 {
            let input = step as f32 / 200_000.0;
            assert!(
                gamma_tone_pack(input, table).abs_diff(analytical_gamma_tone_pack(input)) <= 1,
                "input {input} exceeded the one-level error bound"
            );
        }

        for input in [
            f32::NEG_INFINITY,
            -1.0,
            0.0,
            1.0,
            2.0,
            f32::INFINITY,
            f32::NAN,
        ] {
            assert_eq!(
                gamma_tone_pack(input, table),
                analytical_gamma_tone_pack(input)
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "the rawler reference path uses Rayon")]
    fn direct_superpixel_matches_rawler_for_all_bayer_patterns_and_offset_roi() {
        let width = 14;
        let height = 12;
        let mosaic = PixF32::new_with(
            (0..width * height)
                .map(|index| index as f32 * 0.003_906_25)
                .collect(),
            width,
            height,
        );
        let roi = Rect::new(Point::new(1, 1), Dim2::new(11, 9));
        let colors = PlaneColor::new("RGB");

        for pattern in ["RGGB", "BGGR", "GBRG", "GRBG"] {
            let cfa = CFA::new(pattern);
            let expected = Superpixel3Channel::new().demosaic(&mosaic, &cfa, &colors, roi);
            let actual = super::superpixel_demosaic(&mosaic, &cfa, &colors, roi);
            assert_eq!(actual.width, expected.width, "{pattern} width");
            assert_eq!(actual.height, expected.height, "{pattern} height");
            assert_eq!(actual.data, expected.data, "{pattern} pixels");
        }
    }

    #[cfg(miri)]
    #[test]
    fn superpixel_output_initialization_is_valid_under_miri() {
        let mosaic = PixF32::new_with((0..16).map(|value| value as f32).collect(), 4, 4);
        let actual = super::superpixel_demosaic(
            &mosaic,
            &CFA::new("RGGB"),
            &PlaneColor::new("RGB"),
            mosaic.rect(),
        );

        assert_eq!((actual.width, actual.height), (2, 2));
        assert_eq!(
            actual.data,
            vec![
                [0.0, 2.5, 5.0],
                [2.0, 4.5, 7.0],
                [8.0, 10.5, 13.0],
                [10.0, 12.5, 15.0],
            ]
        );
    }

    #[test]
    fn develop_region_matches_full_and_browse_crop_geometry() {
        let active = Rect::new(Point::new(10, 20), Dim2::new(100, 80));
        let crop = Rect::new(Point::new(14, 26), Dim2::new(90, 60));

        assert_eq!(
            super::develop_region(Dim2::new(100, 80), None, Some(active), super::Quality::Full),
            Rect::new(Point::zero(), Dim2::new(100, 80))
        );
        assert_eq!(
            super::develop_region(
                Dim2::new(100, 80),
                Some(crop),
                Some(active),
                super::Quality::Full,
            ),
            Rect::new(Point::new(4, 6), Dim2::new(90, 60))
        );
        assert_eq!(
            super::develop_region(
                Dim2::new(50, 40),
                Some(crop),
                Some(active),
                super::Quality::Browse,
            ),
            Rect::new(Point::new(2, 3), Dim2::new(45, 30))
        );
        assert_eq!(
            super::develop_region(Dim2::new(7, 5), None, None, super::Quality::Browse),
            Rect::new(Point::zero(), Dim2::new(7, 5))
        );
    }

    #[test]
    fn strided_region_mutation_and_pack_match_a_contiguous_crop() {
        let width = 6;
        let height = 5;
        let data: Vec<[f32; 3]> = (0..width * height)
            .map(|index| [index as f32, index as f32 + 0.25, index as f32 + 0.75])
            .collect();
        let mut strided = super::Color2D::new_with(data, width, height);
        let region = Rect::new(Point::new(2, 1), Dim2::new(3, 3));
        let mut contiguous = strided.crop(region);

        super::for_each_region_pixel_mut(&mut strided, region, |pixel| {
            pixel[0] += 40.0;
            pixel[1] += 40.0;
            pixel[2] += 40.0;
        });
        contiguous.data.iter_mut().for_each(|pixel| {
            pixel[0] += 40.0;
            pixel[1] += 40.0;
            pixel[2] += 40.0;
        });

        let mut actual = vec![0; region.width() * region.height() * 4];
        let mut expected = vec![0; contiguous.data.len() * 4];
        super::pack_region_rgba(&strided, region, &mut actual, |value| value as u8);
        super::pack_region_rgba(&contiguous, contiguous.rect(), &mut expected, |value| {
            value as u8
        });
        assert_eq!(actual, expected);

        for y in 0..height {
            for x in 0..width {
                if x < region.x()
                    || x >= region.x() + region.width()
                    || y < region.y()
                    || y >= region.y() + region.height()
                {
                    assert_eq!(strided.data[y * width + x][0], (y * width + x) as f32);
                }
            }
        }
    }

    #[test]
    #[ignore = "requires the pinned public-domain Sony RAW fixture or VIEWR_TEST_RAW"]
    fn real_sony_raw_lut_output_never_differs_by_more_than_one_level() {
        let path = real_sony_raw_fixture();

        for quality in [super::Quality::Browse, super::Quality::Full] {
            let raw = crate::decode::load(&path).expect("fixture decodes").raw;
            let (analytical, _) =
                super::develop_with_gamma(raw.clone(), quality, super::GammaMode::Analytical)
                    .expect("analytical develop succeeds");
            let (lut, _) = super::develop_with_gamma(raw, quality, super::GammaMode::Lut)
                .expect("LUT develop succeeds");

            assert_eq!(
                (lut.width, lut.height),
                (analytical.width, analytical.height)
            );
            let mut changed = 0usize;
            let mut max_difference = 0u8;
            for (&actual, &expected) in lut.rgba.iter().zip(&analytical.rgba) {
                let difference = actual.abs_diff(expected);
                changed += usize::from(difference != 0);
                max_difference = max_difference.max(difference);
            }
            eprintln!(
                "{quality:?}: {changed}/{} channels changed; max difference {max_difference}",
                lut.rgba.len()
            );
            assert!(max_difference <= 1);
        }
    }

    /// Deterministic pseudo-random values for the pack-fusion unit test.
    fn synthetic_develop_frame(width: usize, height: usize) -> super::Color2D<f32, 3> {
        let mut state = 0x1234_5678_9ABC_DEF0_u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 40) & 0xFFFF) as f32 / 65536.0
        };
        let data = (0..width * height)
            .map(|_| [next(), next(), next()])
            .collect();
        super::Color2D::new_with(data, width, height)
    }

    /// A cover of `(out_w, out_h)`: a "visible" rectangle first, then
    /// deterministic uneven full-width row bands spanning the whole frame.
    /// Overlaps are intentional — overlapping writes must be byte-identical.
    fn covering_regions(out_w: usize, out_h: usize) -> Vec<Rect> {
        let mut regions = Vec::new();
        let visible = Rect::new(
            Point::new(out_w / 3, out_h / 3),
            Dim2::new((out_w / 2).max(1), (out_h / 2).max(1)),
        );
        regions.push(visible);
        let mut state = 0xDEAD_BEEF_u64;
        let mut y = 0;
        while y < out_h {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let height = (1 + (state >> 33) as usize % 1024).min(out_h - y);
            regions.push(Rect::new(Point::new(0, y), Dim2::new(out_w, height)));
            y += height;
        }
        regions
    }

    #[test]
    fn fused_oriented_pack_matches_pack_then_apply_orient() {
        use crate::types::Orient;

        let width = 13;
        let height = 9;
        let image = synthetic_develop_frame(width, height);
        let pack = |value: f32| (value * 255.0) as u8;

        let mut reference_flat = vec![0u8; width * height * 4];
        super::pack_region_rgba(&image, image.rect(), &mut reference_flat, pack);

        for orient in [Orient::R0, Orient::R90, Orient::R180, Orient::R270] {
            let reference = crate::resize::apply_orient(
                crate::types::PixelBuf::new_opaque(
                    width as u32,
                    height as u32,
                    reference_flat.clone(),
                ),
                orient,
            );
            let (dw, dh) = if orient.swaps_axes() {
                (height, width)
            } else {
                (width, height)
            };
            assert_eq!((reference.width, reference.height), (dw as u32, dh as u32));

            // Whole-frame canvas assembled from overlapping sub-rectangles.
            let mut canvas = vec![0u8; dw * dh * 4];
            for rect in covering_regions(width, height) {
                let sub = image.crop(rect);
                let display = super::pack_oriented_region(
                    &sub,
                    sub.rect(),
                    rect,
                    image.dim(),
                    orient,
                    &mut canvas,
                    dw as u32,
                    [0, 0],
                    pack,
                );
                assert_eq!(
                    display,
                    super::oriented_display_rect(rect, image.dim(), orient),
                    "{orient:?} display rect"
                );
            }
            assert_eq!(canvas, reference.rgba, "{orient:?} whole-frame canvas");

            // Region-local scratch canvases must carry the same bytes as the
            // corresponding reference window.
            for rect in covering_regions(width, height) {
                let sub = image.crop(rect);
                let [dx, dy, drw, drh] = super::oriented_display_rect(rect, image.dim(), orient);
                let mut scratch = vec![0u8; drw as usize * drh as usize * 4];
                super::pack_oriented_region(
                    &sub,
                    sub.rect(),
                    rect,
                    image.dim(),
                    orient,
                    &mut scratch,
                    drw,
                    [dx, dy],
                    pack,
                );
                for row in 0..drh as usize {
                    let scratch_row =
                        &scratch[row * drw as usize * 4..(row + 1) * drw as usize * 4];
                    let start = ((dy as usize + row) * dw + dx as usize) * 4;
                    assert_eq!(
                        scratch_row,
                        &reference.rgba[start..start + drw as usize * 4],
                        "{orient:?} scratch row {row} of {rect:?}"
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires the pinned public-domain Sony RAW fixture or VIEWR_TEST_RAW"]
    fn real_sony_raw_region_assembly_matches_monolithic_for_all_orients() {
        use crate::types::Orient;

        let path = real_sony_raw_fixture();
        for orient in [Orient::R0, Orient::R90, Orient::R180, Orient::R270] {
            let raw = crate::decode::load(&path).expect("fixture decodes").raw;
            assert!(super::supports_region_develop(&raw));
            let (monolithic, _) =
                super::develop(raw.clone(), super::Quality::Full).expect("monolithic develop");
            let reference = crate::resize::apply_orient(monolithic, orient);

            let plan = super::plan_full_develop(raw).expect("plan");
            let (dw, dh) = plan.display_size(orient);
            assert_eq!((dw, dh), (reference.width, reference.height), "{orient:?}");
            let (out_w, out_h) = plan.output_size();
            let mut canvas = vec![0u8; dw as usize * dh as usize * 4];
            for rect in covering_regions(out_w as usize, out_h as usize) {
                plan.develop_region_into(rect, orient, &mut canvas, dw, [0, 0]);
            }
            assert_eq!(canvas, reference.rgba, "{orient:?} assembled bytes");

            // Identical input bytes must produce identical cache JPEG objects.
            let assembled = crate::types::PixelBuf::new_opaque(dw, dh, canvas);
            let assembled_jpeg = crate::jobs::encode_jpeg(&assembled, 90).expect("jpeg encodes");
            let reference_jpeg = crate::jobs::encode_jpeg(&reference, 90).expect("jpeg encodes");
            assert_eq!(assembled_jpeg, reference_jpeg, "{orient:?} jpeg bytes");
        }
    }

    #[test]
    #[ignore = "requires the local ignored portrait Sony RAW fixture"]
    // Deliberately outside the `real_sony_raw_` prefix: CI's pinned-fixture
    // pass runs that filter against the public landscape fixture, and this
    // test needs the private portrait file next to VIEWR_TEST_RAW.
    fn portrait_arw_region_assembly_matches_display_pipeline() {
        use crate::types::Orient;

        let path = real_sony_raw_fixture().with_file_name("HCA05417.ARW");
        let decoded = crate::decode::load(&path).expect("portrait fixture decodes");
        let meta = crate::meta::FileMeta::from_metadata(&decoded.metadata);
        assert_eq!(meta.orient, Orient::R270, "fixture must be portrait R270");

        let raw = decoded.raw;
        let (monolithic, _) =
            super::develop(raw.clone(), super::Quality::Full).expect("monolithic develop");
        // Exactly the display pipeline's tail: develop, then orient.
        let reference = crate::resize::apply_orient(monolithic, meta.orient);

        let plan = super::plan_full_develop(raw).expect("plan");
        let (dw, dh) = plan.display_size(meta.orient);
        assert_eq!((dw, dh), (reference.width, reference.height));
        assert!(dh > dw, "portrait display must be taller than wide");
        let (out_w, out_h) = plan.output_size();
        let mut canvas = vec![0u8; dw as usize * dh as usize * 4];
        for rect in covering_regions(out_w as usize, out_h as usize) {
            plan.develop_region_into(rect, meta.orient, &mut canvas, dw, [0, 0]);
        }
        assert_eq!(canvas, reference.rgba, "portrait assembled bytes");
    }

    #[test]
    #[ignore = "requires the pinned public-domain Sony RAW fixture or VIEWR_TEST_RAW"]
    fn real_sony_raw_strided_region_matches_copied_crop_exactly() {
        let path = real_sony_raw_fixture();

        for quality in [super::Quality::Browse, super::Quality::Full] {
            let raw = crate::decode::load(&path).expect("fixture decodes").raw;
            eprintln!(
                "sensor {}x{}, active {:?}, crop {:?}",
                raw.width, raw.height, raw.active_area, raw.crop_area
            );
            let strided_start = Instant::now();
            let (strided, strided_timings) = super::develop_with_layout(
                raw.clone(),
                quality,
                super::GammaMode::Lut,
                super::RegionLayout::Strided,
            )
            .expect("strided-region develop succeeds");
            let strided_wall = strided_start.elapsed();
            let copied_start = Instant::now();
            let (copied, copied_timings) = super::develop_with_layout(
                raw,
                quality,
                super::GammaMode::Lut,
                super::RegionLayout::Copied,
            )
            .expect("copied-crop reference develop succeeds");
            let copied_wall = copied_start.elapsed();

            assert_eq!(
                (strided.width, strided.height),
                (copied.width, copied.height)
            );
            assert_eq!(strided.rgba, copied.rgba, "{quality:?} pixels");
            eprintln!(
                "{quality:?}: strided {strided_wall:?} {strided_timings:?}, copied {copied_wall:?} {copied_timings:?}; removed {} MiB RGB crop copy",
                strided.rgba.len() / 4 * std::mem::size_of::<[f32; 3]>() / (1024 * 1024),
            );
        }
    }
}
