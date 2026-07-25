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
use rawler::imgop::raw::clip_euclidean_norm_avg;
use rawler::imgop::sensor::bayer::{Demosaic, ppg::PPGDemosaic, superpixel::Superpixel3Channel};
use rawler::imgop::srgb;
use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};
use rawler::imgop::{Dim2, Point, Rect};
use rawler::pixarray::{Color2D, PixF32};
use rawler::rawimage::{RawImageData, RawPhotometricInterpretation};
use rawler::{CFA, RawImage};
use rayon::prelude::*;

use crate::types::PixelBuf;

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
/// [`DevelopError::Rawler`] when RAW normalization fails.
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
    mut raw: RawImage,
    quality: Quality,
    gamma_mode: GammaMode,
    region_layout: RegionLayout,
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
    let xyz2cam = raw
        .color_matrix
        .iter()
        .find(|(illuminant, _)| **illuminant == Illuminant::D65)
        .or_else(|| raw.color_matrix.iter().next())
        .map(|(_, m)| m.clone());
    if let Some(color_matrix) = xyz2cam.filter(|matrix| usable_color_matrix(matrix)) {
        let mut matrix: [[f32; 3]; 4] = [[0.0; 3]; 4];
        for (i, row) in matrix.iter_mut().enumerate().take(color_matrix.len() / 3) {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = color_matrix[i * 3 + j];
            }
        }
        let cam2rgb = pseudo_inverse(normalize(multiply(&matrix, &SRGB_TO_XYZ_D65)));
        if cam2rgb.iter().flatten().all(|value| value.is_finite()) {
            let wb = sanitize_white_balance(raw.wb_coeffs);
            for_each_region_pixel_mut(&mut image, region, |pix| {
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

    Ok((
        PixelBuf {
            width: out_w,
            height: out_h,
            rgba,
        },
        timings,
    ))
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

/// Fixed "camera-standard-ish" base tone curve: a mild S in display
/// space (smoothstep blend) so renders match the punch of camera JPEGs
/// instead of looking flat. No editing UI — it's a culler; the constant
/// is versioned by `cache_disk::DEVELOP_VERSION`.
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
    let gamma = srgb::srgb_apply_gamma(v);
    (base_curve(gamma.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8
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
fn base_curve(v: f32) -> f32 {
    let s = v * v * (3.0 - 2.0 * v); // smoothstep
    v + TONE_STRENGTH * (s - v)
}

#[cfg(test)]
mod tests {
    use super::{
        DevelopTimings, analytical_gamma_tone_pack, base_curve, gamma_tone_pack,
        sanitize_white_balance, total, usable_color_matrix,
    };
    use rawler::CFA;
    use rawler::cfa::PlaneColor;
    use rawler::imgop::sensor::bayer::{Demosaic, superpixel::Superpixel3Channel};
    use rawler::imgop::{Dim2, Point, Rect};
    use rawler::pixarray::PixF32;
    use std::time::{Duration, Instant};

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
    #[ignore = "requires the local ignored 36 MB Sony RAW fixture"]
    fn real_sony_raw_lut_output_never_differs_by_more_than_one_level() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/real-raw-corpus/HCA04875.ARW");

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

    #[test]
    #[ignore = "requires the local ignored 36 MB Sony RAW fixture"]
    fn real_sony_raw_strided_region_matches_copied_crop_exactly() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/real-raw-corpus/HCA04875.ARW");

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
