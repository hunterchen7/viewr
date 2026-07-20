//! M1 app: folder browsing with filmstrip, keyboard nav, progressive
//! loupe (thumb → browse → full), zoom/pan.
//!
//! Interim worker model until the M2 scheduler lands: one loupe worker
//! (decodes the current image, aborting between stages when the target
//! moves) and one thumb worker (walks the folder outward from the start
//! index). Workers only produce PixelBufs; textures are UI-thread-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use anyhow::{Result, anyhow};
use eframe::egui::{self, vec2};
use viewr_core::develop::{Quality, develop};
use viewr_core::folder::{FolderEntry, outward_order, scan};
use viewr_core::meta::FileMeta;
use viewr_core::resize::apply_orient;
use viewr_core::types::PixelBuf;

use crate::loupe::{self, Zoom};

const THUMB_EDGE: u32 = 360;

enum Event {
    Thumb {
        index: usize,
        buf: PixelBuf,
        meta: Box<FileMeta>,
    },
    ThumbFailed {
        index: usize,
        error: String,
    },
    Loupe {
        index: usize,
        quality: Quality,
        buf: PixelBuf,
        started: Instant,
    },
    LoupeFailed {
        index: usize,
        error: String,
    },
}

struct LoupeReq {
    index: usize,
    path: PathBuf,
    generation: u64,
}

pub fn run(dir: &Path, select: Option<&Path>) -> Result<()> {
    let entries = scan(dir)?;
    if entries.is_empty() {
        return Err(anyhow!("no raw files found in {}", dir.display()));
    }
    let start = select
        .and_then(|f| entries.iter().position(|e| e.path == f))
        .unwrap_or(0);
    let title = format!("viewr — {}", dir.display());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1500.0, 950.0]),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let app = App::new(cc, entries, start);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

pub struct App {
    entries: Arc<Vec<FolderEntry>>,
    current: usize,
    generation: Arc<AtomicU64>,
    loupe_tx: Sender<LoupeReq>,
    events: Receiver<Event>,

    thumbs: HashMap<usize, egui::TextureHandle>,
    metas: HashMap<usize, FileMeta>,
    thumb_errors: usize,

    /// (index, quality) the main texture currently shows.
    main_shows: Option<(usize, Quality)>,
    main_tex: Option<egui::TextureHandle>,
    zoom: Zoom,
    status: String,
    scroll_to_current: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, entries: Vec<FolderEntry>, start: usize) -> Self {
        let entries = Arc::new(entries);
        let (event_tx, events) = std::sync::mpsc::channel::<Event>();
        let (loupe_tx, loupe_rx) = std::sync::mpsc::channel::<LoupeReq>();
        let generation = Arc::new(AtomicU64::new(0));

        // Loupe worker.
        {
            let tx = event_tx.clone();
            let ctx = cc.egui_ctx.clone();
            let generation = generation.clone();
            std::thread::spawn(move || loupe_worker(&loupe_rx, &tx, &ctx, &generation));
        }
        // Thumb worker: outward from the starting image.
        {
            let tx = event_tx;
            let ctx = cc.egui_ctx.clone();
            let entries = entries.clone();
            std::thread::spawn(move || {
                for index in outward_order(entries.len(), start) {
                    let event = match viewr_core::decode::thumb_and_meta(
                        &entries[index].path,
                        THUMB_EDGE,
                    ) {
                        Ok(r) => Event::Thumb {
                            index,
                            buf: r.thumb,
                            meta: Box::new(r.meta),
                        },
                        Err(e) => Event::ThumbFailed {
                            index,
                            error: e.to_string(),
                        },
                    };
                    if tx.send(event).is_err() {
                        return; // UI gone
                    }
                    ctx.request_repaint();
                }
            });
        }

        let mut app = Self {
            entries,
            current: start,
            generation,
            loupe_tx,
            events,
            thumbs: HashMap::new(),
            metas: HashMap::new(),
            thumb_errors: 0,
            main_shows: None,
            main_tex: None,
            zoom: Zoom::Fit,
            status: String::new(),
            scroll_to_current: true,
        };
        app.request_current();
        app
    }

    fn request_current(&mut self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.loupe_tx.send(LoupeReq {
            index: self.current,
            path: self.entries[self.current].path.clone(),
            generation,
        });
    }

    fn navigate(&mut self, delta: isize) {
        let len = self.entries.len() as isize;
        let next = (self.current as isize + delta).clamp(0, len - 1) as usize;
        self.select(next);
    }

    fn select(&mut self, index: usize) {
        if index == self.current {
            return;
        }
        self.current = index;
        self.zoom = Zoom::Fit;
        self.scroll_to_current = true;
        self.request_current();
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Thumb { index, buf, meta } => {
                    let image = to_color_image(&buf);
                    self.thumbs.insert(
                        index,
                        ctx.load_texture(
                            format!("thumb{index}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        ),
                    );
                    self.metas.insert(index, *meta);
                }
                Event::ThumbFailed { index, error } => {
                    eprintln!("thumb {index}: {error}");
                    self.thumb_errors += 1;
                }
                Event::Loupe {
                    index,
                    quality,
                    buf,
                    started,
                } => {
                    if index != self.current {
                        continue; // stale
                    }
                    // Don't replace a Full result with a late Browse one.
                    if quality == Quality::Browse && self.main_shows == Some((index, Quality::Full))
                    {
                        continue;
                    }
                    let image = to_color_image(&buf);
                    self.main_tex =
                        Some(ctx.load_texture("main", image, egui::TextureOptions::LINEAR));
                    self.main_shows = Some((index, quality));
                    self.status = format!(
                        "{} — {} {}x{} in {:?}",
                        self.entries[index].file_name,
                        match quality {
                            Quality::Browse => "browse",
                            Quality::Full => "full",
                        },
                        buf.width,
                        buf.height,
                        started.elapsed(),
                    );
                }
                Event::LoupeFailed { index, error } => {
                    if index == self.current {
                        self.status = format!("error: {error}");
                    }
                }
            }
        }
    }

    fn handle_keys(
        &mut self,
        ctx: &egui::Context,
        loupe_rect: egui::Rect,
        img_size: Option<egui::Vec2>,
    ) {
        let (right, left, shift, home, end, toggle) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowLeft),
                i.modifiers.shift,
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
                i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Z),
            )
        });
        let step = if shift { 10 } else { 1 };
        if right {
            self.navigate(step);
        }
        if left {
            self.navigate(-step);
        }
        if home {
            self.select(0);
        }
        if end {
            self.select(self.entries.len() - 1);
        }
        if toggle && let Some(size) = img_size {
            let anchor = ctx
                .pointer_hover_pos()
                .filter(|p| loupe_rect.contains(*p))
                .unwrap_or_else(|| loupe_rect.center());
            loupe::toggle_100(&mut self.zoom, loupe_rect, size, anchor);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);

        // Filmstrip along the bottom.
        let current = self.current;
        let mut clicked: Option<usize> = None;
        egui::Panel::bottom("filmstrip")
            .exact_size(112.0)
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            for (i, _entry) in self.entries.iter().enumerate() {
                                let selected = i == current;
                                let response = match self.thumbs.get(&i) {
                                    Some(tex) => {
                                        let size = tex.size_vec2();
                                        let h = 92.0;
                                        let w = (size.x / size.y * h).clamp(30.0, 180.0);
                                        ui.add(
                                            egui::Button::image(egui::Image::new((
                                                tex.id(),
                                                vec2(w, h),
                                            )))
                                            .selected(selected),
                                        )
                                    }
                                    None => ui.add_sized(
                                        vec2(120.0, 92.0),
                                        egui::Button::new(egui::RichText::new("…").weak())
                                            .selected(selected),
                                    ),
                                };
                                if response.clicked() {
                                    clicked = Some(i);
                                }
                                if selected && self.scroll_to_current {
                                    response.scroll_to_me(Some(egui::Align::Center));
                                }
                            }
                        });
                    });
            });
        self.scroll_to_current = false;
        if let Some(i) = clicked {
            self.select(i);
        }

        // Status line.
        egui::Panel::top("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{}/{}  {}",
                    self.current + 1,
                    self.entries.len(),
                    self.entries[self.current].file_name
                ));
                if let Some(meta) = self.metas.get(&self.current) {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(iso) = meta.iso {
                        parts.push(format!("ISO {iso}"));
                    }
                    if let Some(s) = &meta.shutter {
                        parts.push(s.clone());
                    }
                    if let Some(a) = &meta.aperture {
                        parts.push(a.clone());
                    }
                    if let Some(f) = meta.focal_mm {
                        parts.push(format!("{f:.0}mm"));
                    }
                    ui.separator();
                    ui.label(parts.join("  "));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(&self.status).weak());
                });
            });
        });

        // Main loupe area fills the remaining central space.
        let loupe_rect = ui.available_rect_before_wrap();
        let mut img_size = None;
        match (&self.main_tex, self.main_shows) {
            (Some(tex), Some((index, _quality))) if index == self.current => {
                let size = tex.size_vec2();
                img_size = Some(size);
                let tex = tex.clone();
                let response = loupe::show(ui, &tex, size, &mut self.zoom);
                if let Some(pos) = response.double_clicked_at {
                    loupe::toggle_100(&mut self.zoom, loupe_rect, size, pos);
                }
            }
            _ => {
                // Waiting for develop: show the thumb blown up as a placeholder.
                if let Some(tex) = self.thumbs.get(&self.current) {
                    let size = tex.size_vec2();
                    let tex = tex.clone();
                    loupe::show(ui, &tex, size, &mut Zoom::Fit);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            }
        }

        self.handle_keys(&ctx, loupe_rect, img_size);
    }
}

fn to_color_image(buf: &PixelBuf) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([buf.width as usize, buf.height as usize], &buf.rgba)
}

/// Decode + develop the requested image, browse then full, skipping stale
/// work whenever the UI has already moved on (generation mismatch).
fn loupe_worker(
    rx: &Receiver<LoupeReq>,
    tx: &Sender<Event>,
    ctx: &egui::Context,
    generation: &AtomicU64,
) {
    while let Ok(mut req) = rx.recv() {
        // Collapse to the newest pending request.
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        let stale = || generation.load(Ordering::SeqCst) != req.generation;
        let send = |event: Event| {
            let _ = tx.send(event);
            ctx.request_repaint();
        };
        let started = Instant::now();

        let decoded = match viewr_core::decode::load(&req.path) {
            Ok(d) => d,
            Err(e) => {
                send(Event::LoupeFailed {
                    index: req.index,
                    error: e.to_string(),
                });
                continue;
            }
        };
        let meta = FileMeta::from_metadata(&decoded.metadata);
        if stale() {
            continue;
        }

        let raw_for_full = decoded.raw.clone();
        match develop(decoded.raw, Quality::Browse) {
            Ok((buf, _)) => send(Event::Loupe {
                index: req.index,
                quality: Quality::Browse,
                buf: apply_orient(buf, meta.orient),
                started,
            }),
            Err(e) => {
                send(Event::LoupeFailed {
                    index: req.index,
                    error: e.to_string(),
                });
                continue;
            }
        }
        if stale() {
            continue;
        }
        match develop(raw_for_full, Quality::Full) {
            Ok((buf, _)) => {
                if stale() {
                    continue;
                }
                send(Event::Loupe {
                    index: req.index,
                    quality: Quality::Full,
                    buf: apply_orient(buf, meta.orient),
                    started,
                });
            }
            Err(e) => send(Event::LoupeFailed {
                index: req.index,
                error: e.to_string(),
            }),
        }
    }
}
