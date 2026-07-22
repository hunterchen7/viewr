use std::ops::Range;

use eframe::egui;

pub(crate) const OVERSCAN_COLUMNS: usize = 2;

pub(crate) fn content_width(total: usize, column_width: f32, spacing: f32) -> f32 {
    if total == 0 || column_width <= 0.0 || !column_width.is_finite() || !spacing.is_finite() {
        0.0
    } else {
        (total as f32 * (column_width + spacing) - spacing).max(0.0)
    }
}

pub(crate) fn visible_columns(
    total: usize,
    viewport: Range<f32>,
    column_width: f32,
    spacing: f32,
    overscan: usize,
) -> Range<usize> {
    let stride = column_width + spacing;
    if total == 0
        || stride <= 0.0
        || !stride.is_finite()
        || !viewport.start.is_finite()
        || !viewport.end.is_finite()
    {
        return 0..0;
    }

    let viewport_start = viewport.start.min(viewport.end).max(0.0);
    let viewport_end = viewport.start.max(viewport.end).max(viewport_start);
    let mut first_visible = (viewport_start / stride).floor() as usize;
    let mut end_visible = (viewport_end / stride).ceil() as usize;
    if end_visible > total {
        // A persisted scroll offset can briefly be beyond the end after the
        // folder or filter shrinks. Backfill a full viewport from the tail,
        // as egui's vertical `show_rows` does, instead of drawing two cells.
        let visible_len = end_visible.saturating_sub(first_visible);
        end_visible = total;
        first_visible = total.saturating_sub(visible_len);
    } else {
        first_visible = first_visible.min(total);
    }

    first_visible.saturating_sub(overscan)..end_visible.saturating_add(overscan).min(total)
}

pub(crate) fn centered_scroll_offset(
    total: usize,
    selected: usize,
    column_width: f32,
    spacing: f32,
    viewport_width: f32,
) -> f32 {
    let stride = column_width + spacing;
    if total == 0
        || column_width <= 0.0
        || stride <= 0.0
        || viewport_width <= 0.0
        || !stride.is_finite()
        || !viewport_width.is_finite()
    {
        return 0.0;
    }

    let selected = selected.min(total - 1);
    let selected_center = selected as f32 * stride + column_width / 2.0;
    let max_offset = (content_width(total, column_width, spacing) - viewport_width).max(0.0);
    (selected_center - viewport_width / 2.0).clamp(0.0, max_offset)
}

pub(crate) fn show_columns<R>(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    total: usize,
    column_size: egui::Vec2,
    overscan: usize,
    add_columns: impl FnOnce(&mut egui::Ui, Range<usize>) -> R,
) -> (Range<usize>, R) {
    let spacing = ui.spacing().item_spacing.x;
    let range = visible_columns(
        total,
        viewport.min.x..viewport.max.x,
        column_size.x,
        spacing,
        overscan,
    );
    ui.set_width(content_width(total, column_size.x, spacing));
    ui.set_height(column_size.y);

    let first_x = ui.max_rect().left() + range.start as f32 * (column_size.x + spacing);
    let columns_width = content_width(range.len(), column_size.x, spacing);
    let rect = egui::Rect::from_min_size(
        egui::pos2(first_x, ui.max_rect().top()),
        egui::vec2(columns_width, column_size.y),
    );
    let result = ui
        .scope_builder(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |columns_ui| {
                // Keep automatically generated widget IDs stable as columns enter and
                // leave the viewport. This mirrors egui's vertical `show_rows` helper.
                columns_ui.skip_ahead_auto_ids(range.start);
                add_columns(columns_ui, range.clone())
            },
        )
        .inner;
    (range, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: f32 = 140.0;
    const SPACING: f32 = 8.0;
    const STRIDE: f32 = WIDTH + SPACING;

    #[test]
    fn visible_columns_are_bounded_by_the_viewport_not_the_folder() {
        let small = visible_columns(1_000, 10_000.0..11_500.0, WIDTH, SPACING, 2);
        let huge = visible_columns(1_000_000, 10_000.0..11_500.0, WIDTH, SPACING, 2);

        assert_eq!(small, huge);
        assert!(huge.len() <= (1_500.0_f32 / STRIDE).ceil() as usize + 6);
        assert!(huge.start > 0);
        assert!(huge.end < 1_000);
    }

    #[test]
    fn visible_columns_cover_every_intersecting_cell_and_clamp_edges() {
        assert_eq!(visible_columns(0, 0.0..500.0, WIDTH, SPACING, 2), 0..0);
        assert_eq!(visible_columns(10, -500.0..100.0, WIDTH, SPACING, 2), 0..3);
        assert_eq!(
            visible_columns(10, 9.0 * STRIDE..20.0 * STRIDE, WIDTH, SPACING, 2),
            0..10
        );

        for first in 0..100_usize {
            let viewport = first as f32 * STRIDE + 1.0..(first + 4) as f32 * STRIDE - 1.0;
            let range = visible_columns(100, viewport, WIDTH, SPACING, 2);
            assert!(range.start <= first);
            assert!(range.end >= (first + 4).min(100));
            assert!(range.end <= 100);
        }
    }

    #[test]
    fn visible_columns_backfill_a_viewport_beyond_a_shortened_folder() {
        let range = visible_columns(10, 1_000.0 * STRIDE..1_004.0 * STRIDE, WIDTH, SPACING, 2);
        assert_eq!(range, 4..10);
    }

    #[test]
    fn centered_scroll_offset_handles_first_middle_and_last_columns() {
        assert_eq!(centered_scroll_offset(0, 0, WIDTH, SPACING, 1_000.0), 0.0);
        assert_eq!(centered_scroll_offset(100, 0, WIDTH, SPACING, 1_000.0), 0.0);

        let middle = centered_scroll_offset(100, 50, WIDTH, SPACING, 1_000.0);
        let expected_middle = 50.0 * STRIDE + WIDTH / 2.0 - 500.0;
        assert!((middle - expected_middle).abs() < 0.01);

        let last = centered_scroll_offset(100, 999, WIDTH, SPACING, 1_000.0);
        assert!((last - (content_width(100, WIDTH, SPACING) - 1_000.0)).abs() < 0.01);
    }
}
