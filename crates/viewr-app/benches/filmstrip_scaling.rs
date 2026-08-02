use std::collections::HashMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use eframe::egui;
use viewr_core::types::PixelBuf;

#[allow(dead_code, unused_imports)]
#[path = "../src/filmstrip.rs"]
mod filmstrip;
#[allow(dead_code, unused_imports)]
#[path = "../src/pixels.rs"]
mod pixels;
#[allow(dead_code, unused_imports)]
#[path = "../src/progressive_texture.rs"]
mod progressive_texture;
#[allow(dead_code, unused_imports)]
#[path = "../src/rating_groups.rs"]
mod rating_groups;
#[allow(dead_code, unused_imports)]
#[path = "../src/texture_lru.rs"]
mod texture_lru;

const SCREEN: egui::Vec2 = egui::vec2(1_500.0, 300.0);
const CELL: egui::Vec2 = egui::vec2(140.0, 220.0);

fn input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
        ..Default::default()
    }
}

fn bench_rating_propagation(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_group_primitives");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 100_000] {
        let owners = (0..len)
            .map(|index| Some(PathBuf::from(format!("/photos/{:08}.xmp", index / 2))))
            .collect::<Vec<_>>();
        let members = rating_groups::build_owner_members(&owners);
        let target = len / 2;
        let target_members = members[target]
            .as_deref()
            .expect("paired benchmark owners share a group");
        assert_eq!(target_members.len(), 2);
        let mut ratings = (0..len)
            .map(|index| (index, (index % 6) as u8))
            .collect::<HashMap<_, _>>();
        assert_eq!(ratings.len(), len);

        group.bench_with_input(
            BenchmarkId::new("build_owner_members", len),
            &len,
            |b, _| {
                b.iter(|| {
                    black_box(rating_groups::build_owner_members(black_box(&owners)));
                });
            },
        );
        let mut next_rating = 0_u8;
        group.bench_with_input(
            BenchmarkId::new("install_members_threshold_filter", len),
            &len,
            |b, _| {
                b.iter(|| {
                    next_rating = if next_rating == 0 { 5 } else { 0 };
                    black_box(rating_groups::install_rating_for_members(
                        black_box(&mut ratings),
                        black_box(target_members),
                        black_box(next_rating),
                        |old, new| (old >= 3) != (new >= 3),
                    ));
                });
            },
        );
    }

    group.finish();
}

/// Headless version of the pre-virtualization placeholder path. This is kept
/// in the benchmark as a regression baseline: it deliberately constructs a
/// button for every item in the folder.
fn render_all(ctx: &egui::Context, total: usize) -> usize {
    let mut rendered = 0;
    let offset = filmstrip::centered_scroll_offset(total, total / 2, CELL.x, 8.0, SCREEN.x);
    let _ = ctx.run_ui(input(), |ui| {
        egui::ScrollArea::horizontal()
            .horizontal_scroll_offset(offset)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    for index in 0..total {
                        let response = ui.add_sized(
                            CELL,
                            egui::Button::new(egui::RichText::new("…").weak())
                                .selected(index == total / 2),
                        );
                        black_box(response.rect);
                        rendered += 1;
                    }
                });
            });
    });
    rendered
}

fn render_viewport(ctx: &egui::Context, total: usize) -> usize {
    let mut rendered = 0;
    let offset = filmstrip::centered_scroll_offset(total, total / 2, CELL.x, 8.0, SCREEN.x);
    let _ = ctx.run_ui(input(), |ui| {
        egui::ScrollArea::horizontal()
            .horizontal_scroll_offset(offset)
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                filmstrip::show_columns(
                    ui,
                    viewport,
                    total,
                    CELL,
                    filmstrip::OVERSCAN_COLUMNS,
                    |columns_ui, range| {
                        for index in range {
                            columns_ui.allocate_ui_with_layout(
                                CELL,
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    let response = ui.add_sized(
                                        CELL,
                                        egui::Button::new(egui::RichText::new("…").weak())
                                            .selected(index == total / 2),
                                    );
                                    black_box(response.rect);
                                    rendered += 1;
                                },
                            );
                        }
                    },
                );
            });
    });
    rendered
}

fn bench_filmstrip_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("filmstrip_widgets");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for total in [10_000_usize, 50_000] {
        let all_ctx = egui::Context::default();
        group.bench_with_input(BenchmarkId::new("all_items", total), &total, |b, &total| {
            b.iter(|| assert_eq!(black_box(render_all(&all_ctx, total)), total));
        });

        let viewport_ctx = egui::Context::default();
        group.bench_with_input(
            BenchmarkId::new("viewport_only", total),
            &total,
            |b, &total| {
                b.iter(|| {
                    let rendered = black_box(render_viewport(&viewport_ctx, total));
                    assert!(
                        (1..32).contains(&rendered),
                        "viewport rendered {rendered} columns"
                    );
                });
            },
        );
    }
    group.finish();
}

fn bench_thumbnail_lru(c: &mut Criterion) {
    // 256 MiB holds roughly 773 360x241 RGBA corpus thumbnails. Exercise a
    // deliberately wide 200-item grid viewport against a full resident set.
    let mut cache = texture_lru::ByteLru::new(773);
    for index in 0..773 {
        assert!(cache.insert(index, index, 1));
    }
    c.bench_function("thumbnail_lru/touch_200_of_773", |b| {
        b.iter(|| {
            for index in 300..500 {
                assert!(black_box(cache.touch(index)), "resident key {index}");
            }
        });
    });
}

fn experimental_visible_tiles(
    source: &PixelBuf,
    visible_uv: egui::Rect,
    tile_edge: u32,
) -> Vec<egui::ColorImage> {
    let visible = progressive_texture::visible_pixel_rect(source.width, source.height, visible_uv);
    let first_col = visible.x / tile_edge;
    let last_col = visible.right().saturating_sub(1) / tile_edge;
    let first_row = visible.y / tile_edge;
    let last_row = visible.bottom().saturating_sub(1) / tile_edge;
    let mut images = Vec::new();
    for row in first_row..=last_row {
        for col in first_col..=last_col {
            let core_x = col * tile_edge;
            let core_y = row * tile_edge;
            let core_right = (core_x + tile_edge).min(source.width);
            let core_bottom = (core_y + tile_edge).min(source.height);
            let sample_x = core_x.saturating_sub(1);
            let sample_y = core_y.saturating_sub(1);
            let sample_right = core_right.saturating_add(1).min(source.width);
            let sample_bottom = core_bottom.saturating_add(1).min(source.height);
            let sample_width = sample_right - sample_x;
            let sample_height = sample_bottom - sample_y;
            let mut pixels = Vec::with_capacity(sample_width as usize * sample_height as usize);
            for y in sample_y..sample_bottom {
                let row_start = (y as usize * source.width as usize + sample_x as usize) * 4;
                let row_end = row_start + sample_width as usize * 4;
                pixels.extend(source.rgba[row_start..row_end].chunks_exact(4).map(|rgba| {
                    egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
                }));
            }
            images.push(egui::ColorImage::new(
                [sample_width as usize, sample_height as usize],
                pixels,
            ));
        }
    }
    images
}

fn bench_full_texture_first_visible(c: &mut Criterion) {
    const WIDTH: u32 = 6_000;
    const HEIGHT: u32 = 4_000;
    // The all-127 fill keeps a translucent alpha, exercising the exact
    // per-pixel premultiply fallback. Production photo pixels are opaque, so
    // the paired opaque source measures the representative bulk-copy path.
    let source = PixelBuf {
        width: WIDTH,
        height: HEIGHT,
        rgba: vec![127; WIDTH as usize * HEIGHT as usize * 4],
    };
    let opaque_source = {
        let mut rgba = source.rgba.clone();
        rgba.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
        PixelBuf {
            width: WIDTH,
            height: HEIGHT,
            rgba,
        }
    };
    let visible_uv = egui::Rect::from_center_size(egui::pos2(0.5, 0.5), egui::vec2(0.25, 0.2375));
    let order = progressive_texture::priority_order(WIDTH, HEIGHT, visible_uv);
    let visible_count = progressive_texture::visible_prefix_len(WIDTH, HEIGHT, visible_uv, &order);
    let visible_tiles = order.into_iter().take(visible_count).collect::<Vec<_>>();
    assert_eq!(visible_tiles.len(), 4);
    assert_eq!(
        experimental_visible_tiles(&source, visible_uv, 512).len(),
        12
    );
    assert_eq!(
        experimental_visible_tiles(&source, visible_uv, 2_048).len(),
        2
    );

    let mut group = c.benchmark_group("full_texture_first_visible");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(250));
    group.measurement_time(Duration::from_secs(2));
    group.bench_function("baseline_full_24mp", |b| {
        b.iter(|| {
            black_box(egui::ColorImage::from_rgba_unmultiplied(
                [WIDTH as usize, HEIGHT as usize],
                black_box(&source.rgba),
            ));
        });
    });
    group.bench_function("progressive_visible_four_tiles", |b| {
        b.iter(|| {
            for &tile in &visible_tiles {
                black_box(
                    progressive_texture::color_image(black_box(&source), black_box(tile))
                        .expect("valid benchmark tile"),
                );
            }
        });
    });
    group.bench_function("production_full_24mp_opaque", |b| {
        b.iter(|| {
            black_box(pixels::to_color_image(black_box(&opaque_source)));
        });
    });
    group.bench_function("progressive_visible_four_tiles_opaque", |b| {
        b.iter(|| {
            for &tile in &visible_tiles {
                black_box(
                    progressive_texture::color_image(black_box(&opaque_source), black_box(tile))
                        .expect("valid benchmark tile"),
                );
            }
        });
    });
    for tile_edge in [512_u32, 2_048] {
        group.bench_with_input(
            BenchmarkId::new("experimental_visible_tiles", tile_edge),
            &tile_edge,
            |b, &tile_edge| {
                b.iter(|| {
                    black_box(experimental_visible_tiles(
                        black_box(&source),
                        black_box(visible_uv),
                        black_box(tile_edge),
                    ));
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_filmstrip_scaling,
    bench_thumbnail_lru,
    bench_rating_propagation,
    bench_full_texture_first_visible
);
criterion_main!(benches);
