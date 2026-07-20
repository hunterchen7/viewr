//! M0 bare viewer: open one ARW, show browse tier as soon as it's developed,
//! swap to the full tier when ready. Progressive B→F in miniature.

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use anyhow::{Result, anyhow};
use eframe::egui;
use viewr_core::develop::{Quality, develop};
use viewr_core::types::PixelBuf;

enum Msg {
    Image {
        quality: Quality,
        buf: PixelBuf,
        elapsed_ms: u128,
    },
    Error(String),
}

pub fn run(path: &Path) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = path.to_owned();
    let title = format!(
        "viewr — {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || develop_thread(&path, &tx, &ctx));
            Ok(Box::new(ViewerApp {
                rx,
                texture: None,
                status: "decoding raw…".into(),
            }))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

/// Worker: decode once, develop browse (fast) then full (slow), posting each.
/// Only plain RGBA buffers cross this boundary — textures are built on the UI
/// thread.
fn develop_thread(path: &Path, tx: &Sender<Msg>, ctx: &egui::Context) {
    let send = |msg: Msg| {
        let _ = tx.send(msg);
        ctx.request_repaint();
    };
    let t = Instant::now();
    let decoded = match viewr_core::decode::load(path) {
        Ok(d) => d,
        Err(e) => return send(Msg::Error(e.to_string())),
    };
    let raw_for_full = decoded.raw.clone();

    match develop(decoded.raw, Quality::Browse) {
        Ok((buf, _)) => send(Msg::Image {
            quality: Quality::Browse,
            buf,
            elapsed_ms: t.elapsed().as_millis(),
        }),
        Err(e) => return send(Msg::Error(e.to_string())),
    }
    match develop(raw_for_full, Quality::Full) {
        Ok((buf, _)) => send(Msg::Image {
            quality: Quality::Full,
            buf,
            elapsed_ms: t.elapsed().as_millis(),
        }),
        Err(e) => send(Msg::Error(e.to_string())),
    }
}

struct ViewerApp {
    rx: Receiver<Msg>,
    texture: Option<egui::TextureHandle>,
    status: String,
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Image {
                    quality,
                    buf,
                    elapsed_ms,
                } => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [buf.width as usize, buf.height as usize],
                        &buf.rgba,
                    );
                    self.texture = Some(ui.ctx().load_texture(
                        "main",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.status = format!(
                        "{} — {}x{} @ {elapsed_ms}ms",
                        match quality {
                            Quality::Browse => "browse (half-res)",
                            Quality::Full => "full",
                        },
                        buf.width,
                        buf.height,
                    );
                }
                Msg::Error(e) => self.status = format!("error: {e}"),
            }
        }

        if let Some(texture) = &self.texture {
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new(texture).shrink_to_fit());
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
        }
        egui::Area::new(egui::Id::new("status"))
            .fixed_pos(egui::pos2(8.0, 8.0))
            .show(ui.ctx(), |ui| {
                ui.label(
                    egui::RichText::new(&self.status)
                        .background_color(egui::Color32::from_black_alpha(160)),
                );
            });
    }
}
