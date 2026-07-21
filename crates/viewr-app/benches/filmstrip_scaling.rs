use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use eframe::egui;

#[allow(dead_code, unused_imports)]
#[path = "../src/filmstrip.rs"]
mod filmstrip;

const SCREEN: egui::Vec2 = egui::vec2(1_500.0, 300.0);
const CELL: egui::Vec2 = egui::vec2(140.0, 220.0);

fn input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
        ..Default::default()
    }
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
                    assert!(rendered < 32, "viewport rendered {rendered} columns");
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_filmstrip_scaling);
criterion_main!(benches);
