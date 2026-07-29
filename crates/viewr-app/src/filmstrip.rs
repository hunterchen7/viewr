use std::{collections::HashMap, ops::Range};

use eframe::egui;

pub(crate) const OVERSCAN_COLUMNS: usize = 2;
const STAR_MARKER_HIT_RADIUS: f32 = 4.0;
const MAX_STAR_MARKER_BUCKETS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq)]
struct StarMarkerBucket {
    x: f32,
    visible_position: usize,
    count: usize,
    representative_distance: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StarMarkerCacheKey {
    track_left: u32,
    track_right: u32,
    pixels_per_point: u32,
    total: usize,
    column_width: u32,
    spacing: u32,
    revision: u64,
}

#[derive(Default)]
pub(crate) struct StarMarkerCache {
    key: Option<StarMarkerCacheKey>,
    buckets: Vec<StarMarkerBucket>,
}

pub(crate) struct StarMarkerSpec<'a> {
    pub outer_rect: egui::Rect,
    pub viewport_rect: egui::Rect,
    pub total: usize,
    pub column_width: f32,
    pub spacing: f32,
    pub revision: u64,
    pub positions: &'a [usize],
}

/// Scope egui's single-axis wheel remapping to the horizontal filmstrip.
///
/// egui retains native smoothing, trackpad inertia, direction, and boundary
/// handling. The surrounding scoped UI prevents this preference from changing
/// vertical scrolling elsewhere in the app.
pub(crate) fn configure_vertical_scroll(ui: &mut egui::Ui, enabled: bool) {
    ui.style_mut().always_scroll_the_only_direction = enabled;
}

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

fn star_marker_x(
    track: egui::Rect,
    total: usize,
    visible_position: usize,
    column_width: f32,
    spacing: f32,
) -> Option<f32> {
    let stride = column_width + spacing;
    let width = content_width(total, column_width, spacing);
    if total == 0
        || visible_position >= total
        || stride <= 0.0
        || !stride.is_finite()
        || width <= 0.0
        || !track.width().is_finite()
        || track.width() <= 0.0
    {
        return None;
    }

    let center = visible_position as f32 * stride + column_width / 2.0;
    Some(track.left() + (center / width).clamp(0.0, 1.0) * track.width())
}

fn star_marker_buckets(
    track: egui::Rect,
    pixels_per_point: f32,
    total: usize,
    column_width: f32,
    spacing: f32,
    positions: impl IntoIterator<Item = usize>,
) -> Vec<StarMarkerBucket> {
    if pixels_per_point <= 0.0
        || !pixels_per_point.is_finite()
        || track.width() <= 0.0
        || !track.width().is_finite()
    {
        return Vec::new();
    }

    let requested_buckets = ((track.width() * pixels_per_point).ceil() as usize).saturating_add(1);
    let bucket_count = requested_buckets.clamp(1, MAX_STAR_MARKER_BUCKETS);
    let last_bucket = bucket_count.saturating_sub(1);
    let positions = positions.into_iter();
    let marker_capacity = positions.size_hint().1.unwrap_or(0).min(bucket_count);
    let mut buckets: HashMap<usize, StarMarkerBucket> = HashMap::with_capacity(marker_capacity);

    for visible_position in positions {
        let Some(x) = star_marker_x(track, total, visible_position, column_width, spacing) else {
            continue;
        };
        let fraction = ((x - track.left()) / track.width()).clamp(0.0, 1.0);
        let bucket_index = (fraction * last_bucket as f32).round() as usize;
        let bucket_x = if last_bucket == 0 {
            track.center().x
        } else {
            track.left() + bucket_index as f32 / last_bucket as f32 * track.width()
        };
        let representative_distance = (x - bucket_x).abs();
        match buckets.get_mut(&bucket_index) {
            Some(existing) => {
                existing.count += 1;
                if representative_distance < existing.representative_distance
                    || (representative_distance == existing.representative_distance
                        && visible_position < existing.visible_position)
                {
                    existing.visible_position = visible_position;
                    existing.representative_distance = representative_distance;
                }
            }
            None => {
                buckets.insert(
                    bucket_index,
                    StarMarkerBucket {
                        x: bucket_x,
                        visible_position,
                        count: 1,
                        representative_distance,
                    },
                );
            }
        }
    }

    let mut buckets: Vec<_> = buckets.into_values().collect();
    buckets.sort_unstable_by(|a, b| {
        a.x.total_cmp(&b.x)
            .then_with(|| a.visible_position.cmp(&b.visible_position))
    });
    buckets
}

fn nearest_star_marker(
    buckets: &[StarMarkerBucket],
    pointer: egui::Pos2,
    track: egui::Rect,
    hit_radius: f32,
) -> Option<usize> {
    if !track.contains(pointer) || hit_radius < 0.0 || !hit_radius.is_finite() {
        return None;
    }

    buckets
        .iter()
        .filter_map(|bucket| {
            let distance = (pointer.x - bucket.x).abs();
            (distance <= hit_radius).then_some((distance, bucket.visible_position))
        })
        .min_by(|(distance_a, position_a), (distance_b, position_b)| {
            distance_a
                .total_cmp(distance_b)
                .then_with(|| position_a.cmp(position_b))
        })
        .map(|(_, visible_position)| visible_position)
}

/// Paint bounded star-location markers over egui's native horizontal
/// scrollbar and report a click without claiming the scrollbar interaction.
///
/// The native scrollbar still owns track clicks and handle drags. Reading the
/// completed click here lets the caller correct the offset to the exact
/// thumbnail center without selecting the image.
pub(crate) fn show_star_markers(
    ui: &mut egui::Ui,
    spec: StarMarkerSpec<'_>,
    cache: &mut StarMarkerCache,
) -> Option<usize> {
    let StarMarkerSpec {
        outer_rect,
        viewport_rect,
        total,
        column_width,
        spacing,
        revision,
        positions,
    } = spec;
    let content_width = content_width(total, column_width, spacing);
    if content_width.ceil() <= viewport_rect.width().ceil() {
        return None;
    }

    let scroll = ui.spacing().scroll;
    let bottom = outer_rect.bottom().min(ui.clip_rect().bottom()) - scroll.bar_outer_margin;
    let track = egui::Rect::from_min_max(
        egui::pos2(viewport_rect.left(), bottom - scroll.bar_width),
        egui::pos2(viewport_rect.right(), bottom),
    )
    .intersect(ui.clip_rect());
    if track.width() <= 0.0 || track.height() <= 0.0 {
        return None;
    }

    let pixels_per_point = ui.ctx().pixels_per_point();
    let cache_key = StarMarkerCacheKey {
        track_left: track.left().to_bits(),
        track_right: track.right().to_bits(),
        pixels_per_point: pixels_per_point.to_bits(),
        total,
        column_width: column_width.to_bits(),
        spacing: spacing.to_bits(),
        revision,
    };
    if cache.key != Some(cache_key) {
        cache.buckets = star_marker_buckets(
            track,
            pixels_per_point,
            total,
            column_width,
            spacing,
            positions.iter().copied(),
        );
        cache.key = Some(cache_key);
    }
    let buckets = &cache.buckets;
    let painter = ui.painter();
    for bucket in buckets {
        let (width, color) = if bucket.count > 1 {
            (1.0, egui::Color32::GOLD.gamma_multiply(0.55))
        } else {
            (1.5, egui::Color32::GOLD)
        };
        painter.line_segment(
            [
                egui::pos2(bucket.x, track.center().y + 1.0),
                egui::pos2(bucket.x, track.bottom() - 1.0),
            ],
            egui::Stroke::new(width, color),
        );
    }

    let pointer_over_track = ui.is_enabled() && ui.rect_contains_pointer(track);
    let hover = ui.input(|input| input.pointer.hover_pos());
    if pointer_over_track
        && hover.is_some_and(|pointer| {
            nearest_star_marker(buckets, pointer, track, STAR_MARKER_HIT_RADIUS).is_some()
        })
    {
        ui.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if !pointer_over_track {
        return None;
    }
    let click = ui.input(|input| {
        input
            .pointer
            .primary_clicked()
            .then(|| input.pointer.interact_pos())
            .flatten()
    });
    click.and_then(|pointer| nearest_star_marker(buckets, pointer, track, STAR_MARKER_HIT_RADIUS))
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

    #[test]
    fn hundredth_of_five_hundred_is_near_twenty_percent() {
        let track = egui::Rect::from_min_max(egui::pos2(0.0, 80.0), egui::pos2(1_000.0, 90.0));
        let x = star_marker_x(track, 500, 99, WIDTH, SPACING)
            .expect("the hundredth image has marker geometry");

        assert!((x / track.width() - 0.20).abs() < 0.002);
    }

    #[test]
    fn marker_geometry_uses_cell_centers_and_rejects_invalid_inputs() {
        let track = egui::Rect::from_min_max(egui::pos2(100.0, 80.0), egui::pos2(1_100.0, 90.0));
        let first = star_marker_x(track, 500, 0, WIDTH, SPACING).unwrap();
        let last = star_marker_x(track, 500, 499, WIDTH, SPACING).unwrap();

        assert!(first > track.left());
        assert!(last < track.right());
        assert!(((first - track.left()) - (track.right() - last)).abs() < 0.01);
        assert_eq!(star_marker_x(track, 0, 0, WIDTH, SPACING), None);
        assert_eq!(star_marker_x(track, 500, 500, WIDTH, SPACING), None);
        assert_eq!(star_marker_x(track, 500, 0, 0.0, SPACING), None);
    }

    #[test]
    fn dense_star_markers_are_bounded_by_device_pixel_columns() {
        let track = egui::Rect::from_min_max(egui::pos2(0.0, 80.0), egui::pos2(100.0, 90.0));
        let buckets = star_marker_buckets(track, 2.0, 50_000, WIDTH, SPACING, 0..50_000);

        assert!(buckets.len() <= 201);
        assert_eq!(
            buckets.iter().map(|bucket| bucket.count).sum::<usize>(),
            50_000
        );
    }

    #[test]
    fn dense_marker_representatives_and_hit_testing_are_deterministic() {
        let track = egui::Rect::from_min_max(egui::pos2(0.0, 80.0), egui::pos2(10.0, 90.0));
        let forward = star_marker_buckets(track, 1.0, 100, WIDTH, SPACING, 0..100);
        let reverse = star_marker_buckets(track, 1.0, 100, WIDTH, SPACING, (0..100).rev());

        assert_eq!(forward, reverse);
        let marker = nearest_star_marker(&forward, egui::pos2(5.0, 85.0), track, 5.0)
            .expect("a nearby marker is clickable");
        assert_eq!(
            marker,
            nearest_star_marker(&reverse, egui::pos2(5.0, 85.0), track, 5.0).unwrap()
        );
        assert_eq!(
            nearest_star_marker(&forward, egui::pos2(5.0, 60.0), track, 5.0),
            None
        );
    }

    #[test]
    fn clicking_a_marker_centers_its_thumbnail_without_claiming_scrollbar_input() {
        fn input(events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(300.0, 120.0),
                )),
                events,
                ..Default::default()
            }
        }

        fn frame(
            ctx: &egui::Context,
            events: Vec<egui::Event>,
            cache: &mut StarMarkerCache,
        ) -> (egui::Rect, egui::Rect, Option<usize>, f32) {
            let mut captured = None;
            let _ = ctx.run_ui(input(events), |ui| {
                let outer_rect = ui.available_rect_before_wrap();
                let output = egui::ScrollArea::horizontal()
                    .id_salt("star-marker-click")
                    .scroll_bar_visibility(
                        egui::containers::scroll_area::ScrollBarVisibility::AlwaysVisible,
                    )
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.allocate_space(egui::vec2(content_width(100, WIDTH, SPACING), 60.0));
                    });
                let clicked = show_star_markers(
                    ui,
                    StarMarkerSpec {
                        outer_rect,
                        viewport_rect: output.inner_rect,
                        total: 100,
                        column_width: WIDTH,
                        spacing: SPACING,
                        revision: 1,
                        positions: &[50],
                    },
                    cache,
                );
                let mut state = output.state;
                if let Some(position) = clicked {
                    state.offset.x = centered_scroll_offset(
                        100,
                        position,
                        WIDTH,
                        SPACING,
                        output.inner_rect.width(),
                    );
                    state.store(ui.ctx(), output.id);
                    ui.ctx().request_repaint();
                }
                captured = Some((outer_rect, output.inner_rect, clicked, state.offset.x));
            });
            captured.expect("filmstrip frame was captured")
        }

        let ctx = egui::Context::default();
        let mut cache = StarMarkerCache::default();
        let (outer, viewport, _, _) = frame(&ctx, Vec::new(), &mut cache);
        let scroll = ctx.style_of(egui::Theme::Dark).spacing.scroll;
        let track = egui::Rect::from_min_max(
            egui::pos2(viewport.left(), outer.bottom() - scroll.bar_width),
            egui::pos2(viewport.right(), outer.bottom()),
        );
        let marker = egui::pos2(
            star_marker_x(track, 100, 50, WIDTH, SPACING).unwrap(),
            track.center().y,
        );
        let pointer_event = |pressed| egui::Event::PointerButton {
            pos: marker,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };

        frame(
            &ctx,
            vec![egui::Event::PointerMoved(marker), pointer_event(true)],
            &mut cache,
        );
        let (_, _, clicked, stored_offset) = frame(
            &ctx,
            vec![egui::Event::PointerMoved(marker), pointer_event(false)],
            &mut cache,
        );

        assert_eq!(clicked, Some(50));
        let expected = centered_scroll_offset(100, 50, WIDTH, SPACING, viewport.width());
        assert!((stored_offset - expected).abs() < 0.01);

        let (_, _, _, next_frame_offset) = frame(&ctx, Vec::new(), &mut cache);
        assert!((next_frame_offset - expected).abs() < 0.01);

        let native_click_ctx = egui::Context::default();
        let mut native_click_cache = StarMarkerCache::default();
        frame(&native_click_ctx, Vec::new(), &mut native_click_cache);
        let native_track_click = egui::pos2(track.right() - 20.0, track.center().y);
        let native_event = |pressed| egui::Event::PointerButton {
            pos: native_track_click,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            &native_click_ctx,
            vec![
                egui::Event::PointerMoved(native_track_click),
                native_event(true),
            ],
            &mut native_click_cache,
        );
        let (_, _, non_marker_clicked, native_offset) = frame(
            &native_click_ctx,
            vec![
                egui::Event::PointerMoved(native_track_click),
                native_event(false),
            ],
            &mut native_click_cache,
        );
        assert_eq!(non_marker_clicked, None);
        assert!(native_offset > expected);

        let native_drag_ctx = egui::Context::default();
        let mut native_drag_cache = StarMarkerCache::default();
        frame(&native_drag_ctx, Vec::new(), &mut native_drag_cache);
        let handle_start = egui::pos2(track.left() + 2.0, track.center().y);
        frame(
            &native_drag_ctx,
            vec![
                egui::Event::PointerMoved(handle_start),
                egui::Event::PointerButton {
                    pos: handle_start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut native_drag_cache,
        );
        frame(
            &native_drag_ctx,
            vec![egui::Event::PointerMoved(marker)],
            &mut native_drag_cache,
        );
        let (_, _, drag_clicked, drag_offset) = frame(
            &native_drag_ctx,
            vec![egui::Event::PointerMoved(marker), pointer_event(false)],
            &mut native_drag_cache,
        );
        assert_eq!(drag_clicked, None);
        assert!(drag_offset > 0.0);
    }

    #[test]
    fn vertical_scroll_preference_is_scoped_to_the_filmstrip_ui() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(!ui.style().always_scroll_the_only_direction);
            ui.scope(|ui| {
                configure_vertical_scroll(ui, true);
                assert!(ui.style().always_scroll_the_only_direction);
            });
            assert!(!ui.style().always_scroll_the_only_direction);
        });
    }

    #[test]
    fn vertical_wheel_moves_only_an_enabled_horizontal_filmstrip() {
        fn frame(ctx: &egui::Context, enabled: bool, wheel: egui::Vec2) -> f32 {
            let mut offset = None;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(240.0, 100.0),
                )),
                events: vec![
                    egui::Event::PointerMoved(egui::pos2(50.0, 30.0)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: wheel,
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                configure_vertical_scroll(ui, enabled);
                offset = Some(
                    egui::ScrollArea::horizontal()
                        .id_salt("test-filmstrip")
                        .show(ui, |ui| {
                            ui.allocate_space(egui::vec2(1_000.0, 50.0));
                        })
                        .state
                        .offset
                        .x,
                );
            });
            offset.expect("filmstrip offset is captured")
        }

        let disabled = egui::Context::default();
        let enabled = egui::Context::default();
        let horizontal = egui::Context::default();

        assert_eq!(frame(&disabled, false, egui::vec2(0.0, -80.0)), 0.0);
        assert!(frame(&enabled, true, egui::vec2(0.0, -80.0)) > 0.0);
        assert!(frame(&horizontal, false, egui::vec2(-80.0, 0.0)) > 0.0);
    }
}
