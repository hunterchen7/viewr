use std::collections::HashMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use eframe::egui;

#[allow(dead_code, unused_imports)]
#[path = "../src/filmstrip.rs"]
mod filmstrip;
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

criterion_group!(
    benches,
    bench_filmstrip_scaling,
    bench_thumbnail_lru,
    bench_rating_propagation
);
criterion_main!(benches);
