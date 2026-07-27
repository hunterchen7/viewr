//! Preferences window: input mode, tier border, cache budgets, and
//! click-to-capture keybind editing. Changes save to viewr.toml
//! immediately; cache-profile changes apply on the next folder open.

use eframe::egui;
use viewr_core::jobs::{MAX_CACHE_JPEG_QUALITY, MIN_CACHE_JPEG_QUALITY};

use crate::config::{ACTIONS, Action, Bind, Config, ScrollMode, TierIndicator};

#[derive(Default)]
pub struct SettingsState {
    pub open: bool,
    /// Action currently waiting for a key capture.
    listening: Option<Action>,
}

impl SettingsState {
    /// True while capturing a bind — global key handling should pause.
    pub fn capturing(&self) -> bool {
        self.listening.is_some()
    }

    /// Feed one frame of input; consumes the capture if a key arrived.
    pub fn maybe_capture(&mut self, ctx: &egui::Context, config: &mut Config) {
        let Some(action) = self.listening else { return };
        let captured = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        if let Some((key, mods)) = captured {
            self.listening = None;
            if key != egui::Key::Escape {
                config.add_bind(action, Bind { key, mods });
                config.save();
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, config: &mut Config) {
        if !self.open {
            self.listening = None;
            return;
        }
        let mut open = self.open;
        let mut changed = false;
        egui::Window::new("Preferences")
            .open(&mut open)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.heading("Input");
                ui.horizontal(|ui| {
                    ui.label("Scroll gesture:");
                    changed |= ui
                        .radio_value(&mut config.scroll, ScrollMode::Pan, "pans (pinch zooms)")
                        .changed();
                    changed |= ui
                        .radio_value(&mut config.scroll, ScrollMode::Zoom, "zooms")
                        .changed();
                });
                ui.label(
                    egui::RichText::new("Ctrl/Cmd+scroll always zooms.")
                        .weak()
                        .size(11.0),
                );
                ui.add_space(8.0);

                ui.heading("Display");
                ui.horizontal(|ui| {
                    ui.label("Cache indicator:");
                    changed |= ui
                        .radio_value(&mut config.tier_indicator, TierIndicator::Marks, "marks")
                        .on_hover_text(
                            "Delivery-style marks below thumbnails: green full, amber browse, blue cached",
                        )
                        .changed();
                    changed |= ui
                        .radio_value(&mut config.tier_indicator, TierIndicator::Border, "border")
                        .changed();
                    changed |= ui
                        .radio_value(&mut config.tier_indicator, TierIndicator::Hidden, "hidden")
                        .changed();
                });
                changed |= ui
                    .checkbox(
                        &mut config.show_loading,
                        "Loading message while the current zoom view waits for full resolution",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut config.show_performance,
                        "Performance details (load time and cache sizes)",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut config.show_exposure,
                        "Exposure details in the toolbar",
                    )
                    .changed();
                ui.horizontal(|ui| {
                    ui.label("Grid cell size");
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut config.grid_cell, 120.0..=400.0)
                                .fixed_decimals(0),
                        )
                        .changed();
                });
                ui.label(
                    egui::RichText::new("Filmstrip size: drag its top divider.")
                        .weak()
                        .size(11.0),
                );
                ui.add_space(8.0);

                ui.heading("Cache");
                egui::Grid::new("cache-grid").num_columns(2).show(ui, |ui| {
                    ui.label("RAM budget (GB)");
                    changed |= ui
                        .add(egui::Slider::new(&mut config.ram_gb, 1.0..=16.0).fixed_decimals(1))
                        .changed();
                    ui.end_row();
                    ui.label("Disk budget (GB)");
                    changed |= ui
                        .add(egui::Slider::new(&mut config.disk_gb, 2.0..=200.0).fixed_decimals(0))
                        .changed();
                    ui.end_row();
                    ui.label("JPEG quality");
                    changed |= ui
                        .add(
                            egui::Slider::new(
                                &mut config.jpeg_quality,
                                MIN_CACHE_JPEG_QUALITY..=MAX_CACHE_JPEG_QUALITY,
                            )
                            .integer(),
                        )
                        .on_hover_text(
                            "Higher quality preserves smoother gradients and detail but uses more RAM, disk, and background CPU time",
                        )
                        .changed();
                    ui.end_row();
                });
                ui.label(
                    egui::RichText::new(
                        "80 is smaller/faster · 97 is the default · 100 retains the most detail.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.label(
                    egui::RichText::new(
                        "Cache budgets and JPEG quality apply when the next folder is opened.",
                    )
                        .weak()
                        .size(11.0),
                );
                ui.add_space(8.0);

                ui.heading("Keybinds");
                ui.label(
                    egui::RichText::new(
                        "Click + to capture a new bind (Esc cancels), × to remove.",
                    )
                    .weak()
                    .size(11.0),
                );
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        egui::Grid::new("binds-grid")
                            .num_columns(2)
                            .striped(true)
                            .show(ui, |ui| {
                                for &(_, label, action) in ACTIONS {
                                    ui.label(label);
                                    ui.horizontal(|ui| {
                                        let mut remove: Option<Bind> = None;
                                        for bind in config.binds_of(action).to_vec() {
                                            ui.label(
                                                egui::RichText::new(bind.label())
                                                    .monospace()
                                                    .background_color(ui.visuals().faint_bg_color),
                                            );
                                            if ui
                                                .small_button("×")
                                                .on_hover_text("remove bind")
                                                .clicked()
                                            {
                                                remove = Some(bind);
                                            }
                                        }
                                        if let Some(bind) = remove {
                                            config.remove_bind(action, bind);
                                            changed = true;
                                        }
                                        if self.listening == Some(action) {
                                            ui.label(
                                                egui::RichText::new("press a key…")
                                                    .color(egui::Color32::GOLD),
                                            );
                                        } else if ui
                                            .small_button("+")
                                            .on_hover_text("add bind")
                                            .clicked()
                                        {
                                            self.listening = Some(action);
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
            });
        self.open = open;
        if changed {
            config.save();
        }
    }
}
