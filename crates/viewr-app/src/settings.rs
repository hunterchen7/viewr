//! Preferences window: input mode, display, cache, updates, and keybinds.
//! Viewer preferences save to viewr.toml; updater actions are returned to its
//! cross-process store. Processing and cache-profile changes apply on the next
//! folder open.

use std::num::NonZeroUsize;

use eframe::egui;
use viewr_core::jobs::{MAX_CACHE_JPEG_QUALITY, MIN_CACHE_JPEG_QUALITY};

use crate::config::{
    ACTIONS, Action, Bind, Config, ProcessingThreadLimit, ScrollMode, TierIndicator,
};

fn processing_threads_label(limit: ProcessingThreadLimit, available: usize) -> String {
    match limit {
        ProcessingThreadLimit::Automatic => {
            format!("Automatic ({available} logical CPUs)")
        }
        ProcessingThreadLimit::Limited(limit) if limit.get() > available => {
            format!("{} ({} available)", limit.get(), available)
        }
        ProcessingThreadLimit::Limited(limit) => limit.get().to_string(),
    }
}

#[derive(Default)]
pub struct SettingsActions {
    pub check_for_updates: bool,
    pub show_update_status: bool,
    pub automatic_updates_changed: Option<bool>,
}

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

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        config: &mut Config,
        update_status: &str,
        update_details_available: bool,
        automatic_updates_enabled: bool,
    ) -> SettingsActions {
        let mut actions = SettingsActions::default();
        if !self.open {
            self.listening = None;
            return actions;
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
                changed |= ui
                    .checkbox(
                        &mut config.vertical_scroll_filmstrip,
                        "Use vertical scrolling to move the filmstrip",
                    )
                    .on_hover_text(
                        "Applies only while the pointer is over the bottom filmstrip",
                    )
                    .changed();
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

                ui.heading("Performance");
                let available =
                    viewr_core::jobs::available_worker_threads().get();
                ui.horizontal(|ui| {
                    ui.label("Processing threads");
                    let selected = processing_threads_label(
                        config.processing_threads,
                        available,
                    );
                    let combo = egui::ComboBox::from_id_salt(
                        "processing-thread-limit",
                    )
                    .selected_text(selected)
                    .width(190.0)
                    .height(240.0)
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(
                                &mut config.processing_threads,
                                ProcessingThreadLimit::Automatic,
                                format!(
                                    "Automatic ({available} logical CPUs)"
                                ),
                            )
                            .changed();
                        if let ProcessingThreadLimit::Limited(current) =
                            config.processing_threads
                            && current.get() > available
                        {
                            changed |= ui
                                .selectable_value(
                                    &mut config.processing_threads,
                                    ProcessingThreadLimit::Limited(current),
                                    format!(
                                        "Keep {} (uses {available} here)",
                                        current.get()
                                    ),
                                )
                                .changed();
                            ui.separator();
                        }
                        for worker_threads in 1..=available {
                            let limit = NonZeroUsize::new(worker_threads)
                                .expect("processing thread choice is non-zero");
                            changed |= ui
                                .selectable_value(
                                    &mut config.processing_threads,
                                    ProcessingThreadLimit::Limited(limit),
                                    if worker_threads == 1 {
                                        "1 thread".to_owned()
                                    } else {
                                        format!("{worker_threads} threads")
                                    },
                                )
                                .changed();
                        }
                    });
                    combo.response.on_hover_text(
                        "A fixed value caps CPU-heavy RAW, resize, cache decode, and cache encode work",
                    );
                });
                ui.label(
                    egui::RichText::new(
                        "Automatic keeps maximum throughput. A fixed value limits logical CPU use; interface, metadata, disk I/O, ratings, and update threads remain separate.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.label(
                    egui::RichText::new(
                        "Processing and cache changes apply when the next folder is opened.",
                    )
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
                ui.add_space(8.0);

                ui.heading("Updates");
                ui.label(update_status);
                let mut automatic_updates_enabled = automatic_updates_enabled;
                let automatic_changed = ui
                    .checkbox(
                        &mut automatic_updates_enabled,
                        "Check automatically for stable releases",
                    )
                    .changed();
                if automatic_changed {
                    actions.automatic_updates_changed =
                        Some(automatic_updates_enabled);
                }
                ui.horizontal(|ui| {
                    if ui.button("Check now").clicked() {
                        actions.check_for_updates = true;
                    }
                    if update_details_available
                        && ui.button("Show details").clicked()
                    {
                        actions.show_update_status = true;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Automatic checks run at most once per day across all open Viewr windows.",
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
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processing_thread_labels_distinguish_automatic_and_portable_limits() {
        assert_eq!(
            processing_threads_label(ProcessingThreadLimit::Automatic, 10),
            "Automatic (10 logical CPUs)"
        );
        assert_eq!(
            processing_threads_label(
                ProcessingThreadLimit::Limited(NonZeroUsize::new(4).unwrap()),
                10,
            ),
            "4"
        );
        assert_eq!(
            processing_threads_label(
                ProcessingThreadLimit::Limited(NonZeroUsize::new(12).unwrap()),
                10,
            ),
            "12 (10 available)"
        );
    }
}
