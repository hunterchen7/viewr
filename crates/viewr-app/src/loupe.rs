//! The main image view: fit-to-window or anchored zoom, drag pan,
//! wheel/pinch zoom about the cursor.

use eframe::egui::{self, Pos2, Rect, Sense, Vec2, pos2, vec2};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zoom {
    Fit,
    /// `scale` = screen pixels per image pixel; `center` = image-space UV
    /// (0..1) at the viewport center.
    Anchored {
        scale: f32,
        center: Vec2,
    },
}

pub struct LoupeResponse {
    pub double_clicked_at: Option<Pos2>,
}

/// Draw `texture` into the available space honoring `zoom`.
///
/// `img_size` is the LOGICAL image size (full-resolution dimensions),
/// not the texture's — the layout only samples UVs, so any tier
/// (thumb/browse/full) can back the same framing. Zoom scale therefore
/// always means "screen px per full-res px": 1.0 is true 100% even
/// while a lower tier is standing in.
pub fn show(
    ui: &mut egui::Ui,
    texture: &egui::TextureHandle,
    img_size: Vec2,
    zoom: &mut Zoom,
    scroll_zooms: bool,
) -> LoupeResponse {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let viewport = rect.size();
    let fit_scale = (viewport.x / img_size.x).min(viewport.y / img_size.y);

    if let Some(hover) = response.hover_pos() {
        // zoom_delta covers pinch AND Ctrl/Cmd+scroll (egui folds those
        // in); smooth_scroll_delta is the plain scroll gesture.
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        let pinch = ui.input(|i| i.zoom_delta());
        let factor = if scroll_zooms {
            pinch * (1.0 + scroll.y * 0.003)
        } else {
            pinch
        };
        if (factor - 1.0).abs() > f32::EPSILON {
            let current = match *zoom {
                Zoom::Fit => fit_scale,
                Zoom::Anchored { scale, .. } => scale,
            };
            let new_scale = (current * factor).clamp(fit_scale.min(1.0) * 0.5, 8.0);
            if new_scale <= fit_scale {
                *zoom = Zoom::Fit;
            } else {
                let uv_at_hover = uv_at(rect, img_size, *zoom, fit_scale, hover);
                *zoom = anchored_keeping(rect, img_size, new_scale, uv_at_hover, hover);
            }
        }

        // Plain scroll pans the zoomed image (both axes — trackpads are
        // 2D), matching drag direction.
        if !scroll_zooms
            && scroll != Vec2::ZERO
            && let Zoom::Anchored { scale, center } = *zoom
        {
            let new_center = center
                - vec2(
                    scroll.x / (scale * img_size.x),
                    scroll.y / (scale * img_size.y),
                );
            *zoom = Zoom::Anchored {
                scale,
                center: clamp_center(new_center, viewport, img_size, scale),
            };
        }
    }

    // Drag pan (only meaningful when zoomed in).
    if response.dragged()
        && let Zoom::Anchored { scale, center } = *zoom
    {
        let delta = response.drag_delta();
        let new_center = center
            - vec2(
                delta.x / (scale * img_size.x),
                delta.y / (scale * img_size.y),
            );
        *zoom = Zoom::Anchored {
            scale,
            center: clamp_center(new_center, viewport, img_size, scale),
        };
    }

    // Paint.
    let (draw_rect, uv_rect) = layout(rect, img_size, *zoom, fit_scale);
    ui.painter()
        .with_clip_rect(rect)
        .image(texture.id(), draw_rect, uv_rect, egui::Color32::WHITE);

    LoupeResponse {
        double_clicked_at: response.double_clicked().then(|| {
            response
                .interact_pointer_pos()
                .unwrap_or_else(|| rect.center())
        }),
    }
}

/// Toggle between fit and 100% anchored at `anchor_screen` (viewport coords).
pub fn toggle_100(zoom: &mut Zoom, rect: Rect, img_size: Vec2, anchor_screen: Pos2) {
    let viewport = rect.size();
    let fit_scale = (viewport.x / img_size.x).min(viewport.y / img_size.y);
    *zoom = match *zoom {
        Zoom::Fit => {
            let uv = uv_at(rect, img_size, Zoom::Fit, fit_scale, anchor_screen);
            anchored_keeping(rect, img_size, 1.0, uv, anchor_screen)
        }
        Zoom::Anchored { .. } => Zoom::Fit,
    };
    if let Zoom::Anchored { scale, center } = *zoom {
        *zoom = Zoom::Anchored {
            scale,
            center: clamp_center(center, viewport, img_size, scale),
        };
    }
}

/// Which image UV sits under `screen_pos` given the current layout?
fn uv_at(rect: Rect, img_size: Vec2, zoom: Zoom, fit_scale: f32, screen_pos: Pos2) -> Vec2 {
    let (draw_rect, uv_rect) = layout(rect, img_size, zoom, fit_scale);
    let t = vec2(
        ((screen_pos.x - draw_rect.min.x) / draw_rect.width()).clamp(0.0, 1.0),
        ((screen_pos.y - draw_rect.min.y) / draw_rect.height()).clamp(0.0, 1.0),
    );
    vec2(
        uv_rect.min.x + t.x * uv_rect.width(),
        uv_rect.min.y + t.y * uv_rect.height(),
    )
}

/// Build an anchored zoom such that image UV `uv` lands on screen point
/// `anchor` at the given scale.
fn anchored_keeping(rect: Rect, img_size: Vec2, scale: f32, uv: Vec2, anchor: Pos2) -> Zoom {
    let offset = anchor - rect.center();
    let center = uv
        - vec2(
            offset.x / (scale * img_size.x),
            offset.y / (scale * img_size.y),
        );
    Zoom::Anchored { scale, center }
}

/// Keep the view inside the image (centering axes where the image is
/// smaller than the viewport).
fn clamp_center(center: Vec2, viewport: Vec2, img_size: Vec2, scale: f32) -> Vec2 {
    let half = vec2(
        viewport.x / (2.0 * scale * img_size.x),
        viewport.y / (2.0 * scale * img_size.y),
    );
    let clamp_axis = |c: f32, h: f32| {
        if h >= 0.5 {
            0.5 // image smaller than viewport on this axis → center it
        } else {
            c.clamp(h, 1.0 - h)
        }
    };
    vec2(clamp_axis(center.x, half.x), clamp_axis(center.y, half.y))
}

/// Compute the screen rect to draw into and the UV rect to sample.
fn layout(rect: Rect, img_size: Vec2, zoom: Zoom, fit_scale: f32) -> (Rect, Rect) {
    match zoom {
        Zoom::Fit => {
            let size = img_size * fit_scale;
            let draw = Rect::from_center_size(rect.center(), size);
            (draw, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)))
        }
        Zoom::Anchored { scale, center } => {
            let viewport = rect.size();
            let half = vec2(
                viewport.x / (2.0 * scale * img_size.x),
                viewport.y / (2.0 * scale * img_size.y),
            );
            let uv_min = center - half;
            let uv_max = center + half;
            // Clamp UVs to [0,1]; shrink the draw rect for letterboxed axes.
            let clamped_min = vec2(uv_min.x.max(0.0), uv_min.y.max(0.0));
            let clamped_max = vec2(uv_max.x.min(1.0), uv_max.y.min(1.0));
            let frac_min = vec2(
                (clamped_min.x - uv_min.x) / (uv_max.x - uv_min.x),
                (clamped_min.y - uv_min.y) / (uv_max.y - uv_min.y),
            );
            let frac_max = vec2(
                (clamped_max.x - uv_min.x) / (uv_max.x - uv_min.x),
                (clamped_max.y - uv_min.y) / (uv_max.y - uv_min.y),
            );
            let draw = Rect::from_min_max(
                rect.min + vec2(frac_min.x * viewport.x, frac_min.y * viewport.y),
                rect.min + vec2(frac_max.x * viewport.x, frac_max.y * viewport.y),
            );
            (
                draw,
                Rect::from_min_max(
                    pos2(clamped_min.x, clamped_min.y),
                    pos2(clamped_max.x, clamped_max.y),
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1.0e-5;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    fn assert_vec_close(actual: Vec2, expected: Vec2) {
        assert_close(actual.x, expected.x);
        assert_close(actual.y, expected.y);
    }

    fn assert_pos_close(actual: Pos2, expected: Pos2) {
        assert_close(actual.x, expected.x);
        assert_close(actual.y, expected.y);
    }

    fn assert_rect_close(actual: Rect, expected: Rect) {
        assert_pos_close(actual.min, expected.min);
        assert_pos_close(actual.max, expected.max);
    }

    #[test]
    fn fit_layout_preserves_aspect_ratio_and_centers_letterbox() {
        let viewport = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 600.0));
        let image = vec2(400.0, 400.0);

        let (draw, uv) = layout(viewport, image, Zoom::Fit, 1.5);

        assert_rect_close(
            draw,
            Rect::from_min_max(pos2(110.0, 20.0), pos2(710.0, 620.0)),
        );
        assert_rect_close(uv, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
    }

    #[test]
    fn anchored_layout_crops_large_axis_and_letterboxes_small_axis() {
        let viewport = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 600.0));
        let image = vec2(1000.0, 200.0);
        let zoom = Zoom::Anchored {
            scale: 1.0,
            center: vec2(0.5, 0.5),
        };

        let (draw, uv) = layout(viewport, image, zoom, 0.8);

        assert_rect_close(
            draw,
            Rect::from_min_max(pos2(10.0, 220.0), pos2(810.0, 420.0)),
        );
        assert_rect_close(uv, Rect::from_min_max(pos2(0.1, 0.0), pos2(0.9, 1.0)));
    }

    #[test]
    fn uv_at_maps_fit_content_and_clamps_letterbox_positions() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let image = vec2(400.0, 200.0);

        assert_vec_close(
            uv_at(viewport, image, Zoom::Fit, 2.0, pos2(200.0, 200.0)),
            vec2(0.25, 0.25),
        );
        assert_vec_close(
            uv_at(viewport, image, Zoom::Fit, 2.0, pos2(400.0, 0.0)),
            vec2(0.5, 0.0),
        );
        assert_vec_close(
            uv_at(viewport, image, Zoom::Fit, 2.0, pos2(400.0, 600.0)),
            vec2(0.5, 1.0),
        );
    }

    #[test]
    fn anchored_keeping_places_requested_uv_under_anchor() {
        let viewport = Rect::from_min_size(pos2(100.0, 50.0), vec2(800.0, 600.0));
        let image = vec2(1600.0, 1200.0);
        let requested_uv = vec2(0.7, 0.3);
        let anchor = pos2(650.0, 300.0);
        let zoom = anchored_keeping(viewport, image, 1.25, requested_uv, anchor);

        assert_vec_close(uv_at(viewport, image, zoom, 0.5, anchor), requested_uv);
    }

    #[test]
    fn clamp_center_clamps_cropped_axis_and_centers_small_axis() {
        let viewport = vec2(800.0, 600.0);
        let image = vec2(2000.0, 200.0);

        assert_vec_close(
            clamp_center(vec2(-2.0, 0.9), viewport, image, 1.0),
            vec2(0.2, 0.5),
        );
        assert_vec_close(
            clamp_center(vec2(2.0, 0.1), viewport, image, 1.0),
            vec2(0.8, 0.5),
        );
    }

    #[test]
    fn toggle_100_preserves_anchor_then_returns_to_fit() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let image = vec2(1600.0, 1200.0);
        let anchor = pos2(600.0, 400.0);
        let fit_scale = 0.5;
        let expected_uv = uv_at(viewport, image, Zoom::Fit, fit_scale, anchor);
        let mut zoom = Zoom::Fit;

        toggle_100(&mut zoom, viewport, image, anchor);

        assert!(matches!(
            zoom,
            Zoom::Anchored { scale, .. } if (scale - 1.0).abs() <= EPSILON
        ));
        assert_vec_close(uv_at(viewport, image, zoom, fit_scale, anchor), expected_uv);

        toggle_100(&mut zoom, viewport, image, anchor);
        assert_eq!(zoom, Zoom::Fit);
    }

    #[test]
    fn toggle_100_centers_image_smaller_than_viewport() {
        let viewport = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 600.0));
        let image = vec2(200.0, 100.0);
        let mut zoom = Zoom::Fit;

        toggle_100(&mut zoom, viewport, image, viewport.min);

        assert_eq!(
            zoom,
            Zoom::Anchored {
                scale: 1.0,
                center: vec2(0.5, 0.5),
            }
        );
        let (draw, uv) = layout(viewport, image, zoom, 3.0);
        assert_rect_close(
            draw,
            Rect::from_min_max(pos2(310.0, 270.0), pos2(510.0, 370.0)),
        );
        assert_rect_close(uv, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
    }
}
