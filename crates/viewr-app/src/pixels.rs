//! Shared RGBA8 → [`egui::Color32`] texture-pixel conversion.
//!
//! Production RAW and JPEG buffers are normally opaque, and for alpha 255
//! `Color32::from_rgba_unmultiplied` reduces to a byte-for-byte copy. The
//! converters below use immutable provenance to copy those buffers at memory
//! bandwidth. Arbitrary images, malformed decoder output, and translucent
//! pixels retain the exact per-pixel conversion, so output is byte-identical
//! to `ColorImage::from_rgba_unmultiplied` for every accepted input.

use eframe::egui;
use viewr_core::types::PixelBuf;

/// Pixels examined per bulk-copy decision. The alpha scan touches the same
/// cache lines the copy then consumes, so one chunk stays cheap while a
/// translucent pixel only demotes its own chunk to the per-pixel path.
const CHUNK_PIXELS: usize = 4_096;

/// Appends tightly packed RGBA8 bytes to `pixels`.
///
/// Output is byte-identical to `Color32::from_rgba_unmultiplied` applied per
/// pixel. A trailing partial pixel (storage not a multiple of four bytes) is
/// ignored, matching `chunks_exact`.
pub(crate) fn extend_from_rgba(pixels: &mut Vec<egui::Color32>, rgba: &[u8]) {
    for chunk in rgba.chunks(CHUNK_PIXELS * 4) {
        if chunk.chunks_exact(4).all(|px| px[3] == u8::MAX)
            && let Ok(colors) = bytemuck::try_cast_slice::<u8, egui::Color32>(chunk)
        {
            pixels.extend_from_slice(colors);
            continue;
        }
        pixels.extend(
            chunk
                .chunks_exact(4)
                .map(|px| egui::Color32::from_rgba_unmultiplied(px[0], px[1], px[2], px[3])),
        );
    }
}

/// Appends RGBA8 bytes whose immutable [`PixelBuf`] provenance proves every
/// alpha byte is 255.
///
/// The cast is a byte-for-byte copy into `Color32` storage. A malformed slice
/// still takes [`extend_from_rgba`], preserving the exact safe behavior if an
/// internal invariant is ever broken.
pub(crate) fn extend_from_opaque_rgba(pixels: &mut Vec<egui::Color32>, rgba: &[u8]) {
    if let Ok(colors) = bytemuck::try_cast_slice::<u8, egui::Color32>(rgba) {
        pixels.extend_from_slice(colors);
    } else {
        extend_from_rgba(pixels, rgba);
    }
}

/// Converts a whole [`PixelBuf`] into an egui texture image.
///
/// # Panics
///
/// Panics when the RGBA storage length does not match the dimensions, like
/// `ColorImage::from_rgba_unmultiplied`.
pub(crate) fn to_color_image(buf: &PixelBuf) -> egui::ColorImage {
    let size = [buf.width as usize, buf.height as usize];
    let expected_len = size[0]
        .checked_mul(size[1])
        .and_then(|pixels| pixels.checked_mul(4));
    assert_eq!(
        expected_len,
        Some(buf.rgba().len()),
        "size: {size:?}, rgba.len(): {}",
        buf.rgba().len()
    );
    let mut pixels = Vec::with_capacity(size[0] * size[1]);
    if buf.is_opaque() {
        extend_from_opaque_rgba(&mut pixels, buf.rgba());
    } else {
        extend_from_rgba(&mut pixels, buf.rgba());
    }
    egui::ColorImage::new(size, pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterned_rgba(pixel_count: usize, alpha: impl Fn(usize) -> u8) -> Vec<u8> {
        (0..pixel_count)
            .flat_map(|i| {
                [
                    (i * 7) as u8,
                    (i * 13 + 5) as u8,
                    (i * 29 + 11) as u8,
                    alpha(i),
                ]
            })
            .collect()
    }

    fn assert_matches_egui(width: usize, height: usize, rgba: &[u8], known_opaque: bool) {
        let expected = egui::ColorImage::from_rgba_unmultiplied([width, height], rgba);
        let buf = if known_opaque {
            PixelBuf::try_new_opaque(width as u32, height as u32, rgba.to_vec())
                .expect("test data is opaque")
        } else {
            PixelBuf::new(width as u32, height as u32, rgba.to_vec())
        };
        let actual = to_color_image(&buf);
        assert_eq!(actual.size, expected.size);
        assert_eq!(actual.pixels, expected.pixels);
    }

    #[test]
    fn opaque_buffers_match_the_egui_conversion_across_chunk_boundaries() {
        assert_matches_egui(97, 0, &[], true);
        for pixel_count in [0, 1, 63, CHUNK_PIXELS, CHUNK_PIXELS * 2 + 17] {
            let rgba = patterned_rgba(pixel_count, |_| 255);
            assert_matches_egui(pixel_count, 1, &rgba, true);
            assert_matches_egui(pixel_count, 1, &rgba, false);
        }
    }

    #[test]
    fn translucent_pixels_take_the_exact_premultiply_fallback() {
        // Every alpha class: transparent, low, high, and opaque, positioned so
        // some chunks are fully opaque and others are mixed.
        let pixel_count = CHUNK_PIXELS * 2 + 33;
        let rgba = patterned_rgba(pixel_count, |i| match i {
            i if i == CHUNK_PIXELS + 1 => 0,
            i if i == CHUNK_PIXELS + 2 => 1,
            i if i == CHUNK_PIXELS + 3 => 254,
            i if i % CHUNK_PIXELS == 7 && i > CHUNK_PIXELS => 127,
            _ => 255,
        });
        assert_matches_egui(pixel_count, 1, &rgba, false);

        let all_translucent = patterned_rgba(129, |i| i as u8);
        assert_matches_egui(129, 1, &all_translucent, false);
    }

    #[test]
    fn multi_row_geometry_is_preserved() {
        let width = 640;
        let height = 5;
        let rgba = patterned_rgba(width * height, |i| if i % 1_000 == 999 { 3 } else { 255 });
        assert_matches_egui(width, height, &rgba, false);
    }

    #[test]
    fn direct_opaque_extension_matches_scanned_conversion_exactly() {
        let rgba = patterned_rgba(CHUNK_PIXELS * 2 + 19, |_| 255);
        let mut direct = Vec::new();
        let mut scanned = Vec::new();
        extend_from_opaque_rgba(&mut direct, &rgba);
        extend_from_rgba(&mut scanned, &rgba);
        assert_eq!(direct, scanned);

        // A trailing partial pixel cannot be cast and retains chunks_exact's
        // historical behavior instead of panicking or reading past storage.
        let malformed = rgba[..rgba.len() - 1].to_vec();
        let mut direct = Vec::new();
        let mut scanned = Vec::new();
        extend_from_opaque_rgba(&mut direct, &malformed);
        extend_from_rgba(&mut scanned, &malformed);
        assert_eq!(direct, scanned);
        assert_eq!(direct.len(), malformed.len() / 4);
    }
}
