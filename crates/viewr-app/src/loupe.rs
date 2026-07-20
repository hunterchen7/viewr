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
    /// Screen rect the image was actually drawn into (for overlays).
    pub draw_rect: Rect,
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
        draw_rect: draw_rect.intersect(rect),
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
