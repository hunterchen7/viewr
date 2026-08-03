//! Visible-region-first tiling for Full-resolution GPU textures.

use eframe::egui;
use viewr_core::cache_ram::FullBand;
use viewr_core::types::PixelBuf;

pub(crate) const TILE_EDGE: u32 = 1_024;
pub(crate) const SAMPLE_GUTTER: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TileCoord {
    pub col: u32,
    pub row: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PaintGeometry {
    pub screen: egui::Rect,
    pub texture_uv: egui::Rect,
}

impl PixelRect {
    pub fn right(self) -> u32 {
        self.x + self.width
    }

    pub fn bottom(self) -> u32 {
        self.y + self.height
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

pub(crate) fn core_rect(image_width: u32, image_height: u32, tile: TileCoord) -> Option<PixelRect> {
    let x = tile.col.checked_mul(TILE_EDGE)?;
    let y = tile.row.checked_mul(TILE_EDGE)?;
    if x >= image_width || y >= image_height {
        return None;
    }
    Some(PixelRect {
        x,
        y,
        width: TILE_EDGE.min(image_width - x),
        height: TILE_EDGE.min(image_height - y),
    })
}

pub(crate) fn sample_rect(
    image_width: u32,
    image_height: u32,
    tile: TileCoord,
) -> Option<PixelRect> {
    let core = core_rect(image_width, image_height, tile)?;
    let x = core.x.saturating_sub(SAMPLE_GUTTER);
    let y = core.y.saturating_sub(SAMPLE_GUTTER);
    let right = core.right().saturating_add(SAMPLE_GUTTER).min(image_width);
    let bottom = core
        .bottom()
        .saturating_add(SAMPLE_GUTTER)
        .min(image_height);
    Some(PixelRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

pub(crate) fn visible_pixel_rect(
    image_width: u32,
    image_height: u32,
    visible_uv: egui::Rect,
) -> PixelRect {
    let min_x = visible_uv.min.x.clamp(0.0, 1.0);
    let min_y = visible_uv.min.y.clamp(0.0, 1.0);
    let max_x = visible_uv.max.x.clamp(min_x, 1.0);
    let max_y = visible_uv.max.y.clamp(min_y, 1.0);
    let x = (min_x * image_width as f32).floor() as u32;
    let y = (min_y * image_height as f32).floor() as u32;
    let right = ((max_x * image_width as f32).ceil() as u32).min(image_width);
    let bottom = ((max_y * image_height as f32).ceil() as u32).min(image_height);
    PixelRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// Order Full-resolution tiles under the visible zoom rectangle first, then
/// expand into the surrounding image.
pub(crate) fn priority_order(
    image_width: u32,
    image_height: u32,
    visible_uv: egui::Rect,
) -> Vec<TileCoord> {
    if image_width == 0 || image_height == 0 {
        return Vec::new();
    }
    let cols = image_width.div_ceil(TILE_EDGE);
    let rows = image_height.div_ceil(TILE_EDGE);
    let visible = visible_pixel_rect(image_width, image_height, visible_uv);
    let visible_center_x2 = i64::from(visible.x) * 2 + i64::from(visible.width);
    let visible_center_y2 = i64::from(visible.y) * 2 + i64::from(visible.height);

    let mut tiles = (0..rows)
        .flat_map(|row| (0..cols).map(move |col| TileCoord { col, row }))
        .collect::<Vec<_>>();
    tiles.sort_by_key(|&tile| {
        let core = core_rect(image_width, image_height, tile)
            .expect("enumerated tile must be inside the image");
        let intersects = core.intersects(visible);
        let gap_x = if core.right() <= visible.x {
            visible.x - core.right()
        } else if visible.right() <= core.x {
            core.x - visible.right()
        } else {
            0
        };
        let gap_y = if core.bottom() <= visible.y {
            visible.y - core.bottom()
        } else if visible.bottom() <= core.y {
            core.y - visible.bottom()
        } else {
            0
        };
        let gap_sq = u64::from(gap_x).pow(2) + u64::from(gap_y).pow(2);
        let tile_center_x2 = i64::from(core.x) * 2 + i64::from(core.width);
        let tile_center_y2 = i64::from(core.y) * 2 + i64::from(core.height);
        let center_dx = tile_center_x2 - visible_center_x2;
        let center_dy = tile_center_y2 - visible_center_y2;
        let center_sq = center_dx.unsigned_abs().pow(2) + center_dy.unsigned_abs().pow(2);
        (u8::from(!intersects), gap_sq, center_sq, tile.row, tile.col)
    });
    tiles
}

/// Number of leading entries in [`priority_order`] that overlap the visible
/// image rectangle.
pub(crate) fn visible_prefix_len(
    image_width: u32,
    image_height: u32,
    visible_uv: egui::Rect,
    order: &[TileCoord],
) -> usize {
    let visible = visible_pixel_rect(image_width, image_height, visible_uv);
    order
        .iter()
        .take_while(|&&tile| {
            core_rect(image_width, image_height, tile).is_some_and(|core| core.intersects(visible))
        })
        .count()
}

pub(crate) fn color_image(buf: &PixelBuf, tile: TileCoord) -> Option<egui::ColorImage> {
    let expected_len = usize::try_from(buf.width)
        .ok()?
        .checked_mul(usize::try_from(buf.height).ok()?)?
        .checked_mul(4)?;
    if buf.rgba.len() != expected_len {
        return None;
    }
    let sample = sample_rect(buf.width, buf.height, tile)?;
    let pixel_count = usize::try_from(sample.width)
        .ok()?
        .checked_mul(usize::try_from(sample.height).ok()?)?;
    let mut pixels = Vec::with_capacity(pixel_count);
    let source_stride = usize::try_from(buf.width).ok()?.checked_mul(4)?;
    let row_bytes = usize::try_from(sample.width).ok()?.checked_mul(4)?;
    let first_x = usize::try_from(sample.x).ok()?.checked_mul(4)?;
    for y in sample.y..sample.bottom() {
        let row_start = usize::try_from(y)
            .ok()?
            .checked_mul(source_stride)?
            .checked_add(first_x)?;
        let row = buf.rgba.get(row_start..row_start.checked_add(row_bytes)?)?;
        crate::pixels::extend_from_rgba(&mut pixels, row);
    }
    Some(egui::ColorImage::new(
        [
            usize::try_from(sample.width).ok()?,
            usize::try_from(sample.height).ok()?,
        ],
        pixels,
    ))
}

/// Extracts one tile from a provisional [`FullBand`], or `None` when the
/// band does not fully cover the tile's sample rectangle (gutter included)
/// or its storage is inconsistent.
///
/// For covered tiles the produced image is byte-identical to
/// [`color_image`] over the finished full buffer: the band rows are copies
/// of the same decoded rows, and the sample rectangle is computed against
/// the same full-image dimensions.
pub(crate) fn color_image_band(band: &FullBand, tile: TileCoord) -> Option<egui::ColorImage> {
    if band.buf.width != band.full_width {
        return None;
    }
    let source_stride = usize::try_from(band.full_width).ok()?.checked_mul(4)?;
    let expected_len = source_stride.checked_mul(usize::try_from(band.buf.height).ok()?)?;
    if band.buf.rgba.len() != expected_len {
        return None;
    }
    let sample = sample_rect(band.full_width, band.full_height, tile)?;
    let band_bottom = band.y0.checked_add(band.buf.height)?;
    if sample.y < band.y0 || sample.bottom() > band_bottom {
        return None;
    }
    let pixel_count = usize::try_from(sample.width)
        .ok()?
        .checked_mul(usize::try_from(sample.height).ok()?)?;
    let mut pixels = Vec::with_capacity(pixel_count);
    let row_bytes = usize::try_from(sample.width).ok()?.checked_mul(4)?;
    let first_x = usize::try_from(sample.x).ok()?.checked_mul(4)?;
    for y in sample.y..sample.bottom() {
        let local_row = usize::try_from(y - band.y0).ok()?;
        let row_start = local_row.checked_mul(source_stride)?.checked_add(first_x)?;
        let row = band
            .buf
            .rgba
            .get(row_start..row_start.checked_add(row_bytes)?)?;
        crate::pixels::extend_from_rgba(&mut pixels, row);
    }
    Some(egui::ColorImage::new(
        [
            usize::try_from(sample.width).ok()?,
            usize::try_from(sample.height).ok()?,
        ],
        pixels,
    ))
}

pub(crate) fn paint_geometry(
    image_width: u32,
    image_height: u32,
    tile: TileCoord,
    visible_uv: egui::Rect,
    image_draw_rect: egui::Rect,
) -> Option<PaintGeometry> {
    if image_width == 0
        || image_height == 0
        || visible_uv.width() <= 0.0
        || visible_uv.height() <= 0.0
    {
        return None;
    }
    let normalize = |rect: PixelRect| {
        egui::Rect::from_min_max(
            egui::pos2(
                rect.x as f32 / image_width as f32,
                rect.y as f32 / image_height as f32,
            ),
            egui::pos2(
                rect.right() as f32 / image_width as f32,
                rect.bottom() as f32 / image_height as f32,
            ),
        )
    };
    let core_uv = normalize(core_rect(image_width, image_height, tile)?);
    let sample_uv = normalize(sample_rect(image_width, image_height, tile)?);
    let intersection = core_uv.intersect(visible_uv);
    if intersection.width() <= 0.0 || intersection.height() <= 0.0 {
        return None;
    }
    let map_axis =
        |value: f32, source_min: f32, source_size: f32, target_min: f32, target_size: f32| {
            target_min + (value - source_min) / source_size * target_size
        };
    let screen = egui::Rect::from_min_max(
        egui::pos2(
            map_axis(
                intersection.min.x,
                visible_uv.min.x,
                visible_uv.width(),
                image_draw_rect.min.x,
                image_draw_rect.width(),
            ),
            map_axis(
                intersection.min.y,
                visible_uv.min.y,
                visible_uv.height(),
                image_draw_rect.min.y,
                image_draw_rect.height(),
            ),
        ),
        egui::pos2(
            map_axis(
                intersection.max.x,
                visible_uv.min.x,
                visible_uv.width(),
                image_draw_rect.min.x,
                image_draw_rect.width(),
            ),
            map_axis(
                intersection.max.y,
                visible_uv.min.y,
                visible_uv.height(),
                image_draw_rect.min.y,
                image_draw_rect.height(),
            ),
        ),
    );
    let texture_uv = egui::Rect::from_min_max(
        egui::pos2(
            (intersection.min.x - sample_uv.min.x) / sample_uv.width(),
            (intersection.min.y - sample_uv.min.y) / sample_uv.height(),
        ),
        egui::pos2(
            (intersection.max.x - sample_uv.min.x) / sample_uv.width(),
            (intersection.max.y - sample_uv.min.y) / sample_uv.height(),
        ),
    );
    Some(PaintGeometry { screen, texture_uv })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(width: u32, height: u32) -> PixelBuf {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[x as u8, y as u8, (x + y) as u8, 255]);
            }
        }
        PixelBuf {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn visible_tiles_are_first_then_distance_expands_outward() {
        let visible = egui::Rect::from_min_max(egui::pos2(0.45, 0.40), egui::pos2(0.55, 0.60));
        let order = priority_order(4_096, 3_072, visible);
        let visible_pixels = visible_pixel_rect(4_096, 3_072, visible);
        let visible_count = visible_prefix_len(4_096, 3_072, visible, &order);
        assert_eq!(visible_count, 2);
        assert!(order[..visible_count].iter().all(|&tile| {
            core_rect(4_096, 3_072, tile)
                .expect("tile")
                .intersects(visible_pixels)
        }));

        let outside_gaps = order[visible_count..].iter().map(|&tile| {
            let core = core_rect(4_096, 3_072, tile).expect("tile");
            let dx = if core.right() <= visible_pixels.x {
                visible_pixels.x - core.right()
            } else if visible_pixels.right() <= core.x {
                core.x - visible_pixels.right()
            } else {
                0
            };
            let dy = if core.bottom() <= visible_pixels.y {
                visible_pixels.y - core.bottom()
            } else if visible_pixels.bottom() <= core.y {
                core.y - visible_pixels.bottom()
            } else {
                0
            };
            u64::from(dx).pow(2) + u64::from(dy).pow(2)
        });
        assert!(
            outside_gaps
                .clone()
                .zip(outside_gaps.skip(1))
                .all(|(a, b)| a <= b)
        );
    }

    #[test]
    fn tile_samples_include_a_clamped_filtering_gutter() {
        assert_eq!(
            sample_rect(2_050, 1_100, TileCoord { col: 1, row: 0 }),
            Some(PixelRect {
                x: 1_023,
                y: 0,
                width: 1_026,
                height: 1_025,
            })
        );
        assert_eq!(
            sample_rect(2_050, 1_100, TileCoord { col: 2, row: 1 }),
            Some(PixelRect {
                x: 2_047,
                y: 1_023,
                width: 3,
                height: 77,
            })
        );
    }

    #[test]
    fn extraction_preserves_exact_source_pixels_and_rejects_bad_storage() {
        let source = synthetic(5, 4);
        let image = color_image(&source, TileCoord { col: 0, row: 0 }).expect("tile");
        assert_eq!(image.size, [5, 4]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(0, 0, 0));
        assert_eq!(image.pixels[3 * 5 + 4], egui::Color32::from_rgb(4, 3, 7));

        let mut malformed = source;
        malformed.rgba.pop();
        assert!(color_image(&malformed, TileCoord { col: 0, row: 0 }).is_none());
    }

    fn band_of(full: &PixelBuf, y0: u32, height: u32) -> FullBand {
        let row_bytes = full.width as usize * 4;
        FullBand {
            full_width: full.width,
            full_height: full.height,
            y0,
            buf: PixelBuf {
                width: full.width,
                height,
                rgba: full.rgba
                    [y0 as usize * row_bytes..(y0 + height) as usize * row_bytes]
                    .to_vec(),
            },
        }
    }

    #[test]
    fn band_extraction_equals_full_extraction_for_fully_covered_tiles() {
        let full = synthetic(2_050, 1_100);

        // A top band covering tile row 0 including its bottom sample gutter.
        let top = band_of(&full, 0, TILE_EDGE + SAMPLE_GUTTER);
        // A bottom band starting at tile row 1's gutter row.
        let bottom_y0 = TILE_EDGE - SAMPLE_GUTTER;
        let bottom = band_of(&full, bottom_y0, full.height - bottom_y0);

        for col in 0..3 {
            let row0 = TileCoord { col, row: 0 };
            let row1 = TileCoord { col, row: 1 };

            let from_band = color_image_band(&top, row0).expect("row 0 is fully covered");
            let from_full = color_image(&full, row0).expect("tile");
            assert_eq!(from_band.size, from_full.size, "col {col} row 0 size");
            assert_eq!(from_band.pixels, from_full.pixels, "col {col} row 0 pixels");
            assert!(
                color_image_band(&top, row1).is_none(),
                "col {col}: row 1 samples rows past the top band"
            );

            let from_band = color_image_band(&bottom, row1).expect("row 1 is fully covered");
            let from_full = color_image(&full, row1).expect("tile");
            assert_eq!(from_band.size, from_full.size, "col {col} row 1 size");
            assert_eq!(from_band.pixels, from_full.pixels, "col {col} row 1 pixels");
            assert!(
                color_image_band(&bottom, row0).is_none(),
                "col {col}: row 0 samples rows above the bottom band"
            );
        }
    }

    #[test]
    fn band_extraction_rejects_inconsistent_band_storage() {
        let full = synthetic(2_050, 1_100);
        let tile = TileCoord { col: 0, row: 0 };

        let mut truncated = band_of(&full, 0, TILE_EDGE + SAMPLE_GUTTER);
        truncated.buf.rgba.pop();
        assert!(color_image_band(&truncated, tile).is_none());

        let mut wrong_width = band_of(&full, 0, TILE_EDGE + SAMPLE_GUTTER);
        wrong_width.buf.width -= 1;
        assert!(color_image_band(&wrong_width, tile).is_none());
    }

    #[test]
    fn empty_images_have_no_tiles() {
        for (width, height) in [(0, 10), (10, 0)] {
            let order = priority_order(width, height, egui::Rect::EVERYTHING);
            assert!(order.is_empty());
            assert_eq!(
                visible_prefix_len(width, height, egui::Rect::EVERYTHING, &order),
                0
            );
        }
    }

    #[test]
    fn visible_prefix_contains_every_and_only_intersecting_tile() {
        let visible = egui::Rect::from_min_max(egui::pos2(0.24, 0.24), egui::pos2(0.76, 0.76));
        let order = priority_order(3_072, 3_072, visible);
        let visible_pixels = visible_pixel_rect(3_072, 3_072, visible);
        let prefix_len = visible_prefix_len(3_072, 3_072, visible, &order);

        assert_eq!(prefix_len, 9);
        assert!(order[..prefix_len].iter().all(|&tile| {
            core_rect(3_072, 3_072, tile)
                .expect("tile")
                .intersects(visible_pixels)
        }));
        assert!(order[prefix_len..].iter().all(|&tile| {
            !core_rect(3_072, 3_072, tile)
                .expect("tile")
                .intersects(visible_pixels)
        }));
    }

    #[test]
    fn paint_geometry_maps_core_pixels_and_uses_the_sample_gutter() {
        let visible_uv = egui::Rect::from_min_max(egui::pos2(0.25, 0.0), egui::pos2(0.75, 1.0));
        let draw = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 600.0));
        let geometry = paint_geometry(4_096, 2_048, TileCoord { col: 1, row: 0 }, visible_uv, draw)
            .expect("visible tile");
        assert_eq!(
            geometry.screen,
            egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(410.0, 320.0))
        );
        assert!(geometry.texture_uv.min.x > 0.0);
        assert!(geometry.texture_uv.max.x < 1.0);
        assert_eq!(geometry.texture_uv.min.y, 0.0);
        assert!(geometry.texture_uv.max.y < 1.0);
    }
}
