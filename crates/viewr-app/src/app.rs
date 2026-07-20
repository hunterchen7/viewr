//! The app shell: draws from the RamCache, dispatches navigation to the
//! engine, and owns every texture (workers never touch UI types).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use anyhow::{Result, anyhow};
use eframe::egui::{self, vec2};
use viewr_core::cache_disk::DiskCache;
use viewr_core::cache_ram::RamCache;
use viewr_core::db::{Db, default_db_path};
use viewr_core::folder::{FolderEntry, scan};
use viewr_core::jobs::{Engine, Event, NavState};
use viewr_core::library::{Library, load_ratings};
use viewr_core::meta::FileMeta;
use viewr_core::types::{PixelBuf, Tier};

use crate::loupe::{self, Zoom};

const RGBA_BUDGET: u64 = 3 * 1024 * 1024 * 1024;
const JPEG_BUDGET: u64 = 1536 * 1024 * 1024;
const THUMB_BUDGET: u64 = 384 * 1024 * 1024;

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
        Box::new(move |cc| Ok(Box::new(App::new(cc, entries, start)))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

pub struct App {
    entries: Arc<Vec<FolderEntry>>,
    engine: Engine,
    events: Receiver<Event>,
    cache: Arc<RamCache>,

    current: usize,
    direction: i8,
    zoom: Zoom,

    library: Library,
    ratings: HashMap<usize, u8>,

    thumbs: HashMap<usize, egui::TextureHandle>,
    metas: HashMap<usize, FileMeta>,
    textures: HashMap<(usize, Tier), egui::TextureHandle>,

    nav_started: Option<Instant>,
    status: String,
    scroll_to_current: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, entries: Vec<FolderEntry>, start: usize) -> Self {
        let entries = Arc::new(entries);
        let cache = Arc::new(RamCache::new(THUMB_BUDGET, RGBA_BUDGET, JPEG_BUDGET));
        let disk = DiskCache::open_default();
        let ctx = cc.egui_ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let (engine, events) = Engine::new((*entries).clone(), start, cache.clone(), disk, notify);

        // Resolve ratings: sidecar > DB (embedded arrives later via thumbs).
        let db = default_db_path().and_then(|p| Db::open(&p).ok());
        let ratings = load_ratings(&entries, db.as_ref());
        let library = Library::start();

        let app = Self {
            entries,
            engine,
            events,
            cache,
            current: start,
            direction: 1,
            zoom: Zoom::Fit,
            library,
            ratings,
            thumbs: HashMap::new(),
            metas: HashMap::new(),
            textures: HashMap::new(),
            nav_started: Some(Instant::now()),
            status: String::new(),
            scroll_to_current: true,
        };
        app.replan();
        app
    }

    fn replan(&self) {
        self.engine.navigate(NavState {
            current: self.current,
            direction: self.direction,
            zoomed: !matches!(self.zoom, Zoom::Fit),
        });
    }

    fn navigate(&mut self, delta: isize) {
        let len = self.entries.len() as isize;
        let next = (self.current as isize + delta).clamp(0, len - 1) as usize;
        if delta != 0 {
            self.direction = if delta < 0 { -1 } else { 1 };
        }
        self.select(next);
    }

    fn select(&mut self, index: usize) {
        if index == self.current {
            return;
        }
        self.direction = if index < self.current { -1 } else { 1 };
        self.current = index;
        self.zoom = Zoom::Fit;
        self.scroll_to_current = true;
        self.nav_started = Some(Instant::now());
        self.library.flush(); // navigate-away: push pending sidecar writes
        self.replan();
    }

    fn set_rating(&mut self, rating: u8) {
        let index = self.current;
        if rating == 0 {
            self.ratings.remove(&index);
        } else {
            self.ratings.insert(index, rating);
        }
        self.library.set_rating(&self.entries[index], rating);
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut replan = false;
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::ThumbReady { index, meta } => {
                    if let Some(buf) = self.cache.get_rgba((index, Tier::Thumb)) {
                        self.thumbs.insert(
                            index,
                            ctx.load_texture(
                                format!("thumb{index}"),
                                to_color_image(&buf),
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                    }
                    // Embedded in-camera rating: lowest precedence.
                    if let Some(embedded) = meta.rating
                        && embedded > 0
                        && !self.ratings.contains_key(&index)
                    {
                        self.ratings.insert(index, embedded.min(5) as u8);
                    }
                    self.metas.insert(index, *meta);
                }
                Event::ImageReady { .. } => replan = true,
                Event::ImageFailed { index, tier, error } => {
                    if index == self.current && tier != Tier::Thumb {
                        self.status = format!("error: {error}");
                    } else {
                        eprintln!("job failed {index}/{tier:?}: {error}");
                    }
                }
            }
        }
        if replan {
            self.replan();
        }
    }

    /// Upload policy: current image first (Full then Browse), then at most
    /// one neighbor browse texture per frame; prune textures outside the
    /// keep window.
    fn manage_textures(&mut self, ctx: &egui::Context) {
        let current = self.current;
        self.textures.retain(|(i, tier), _| match tier {
            Tier::Full => *i == current,
            _ => i.abs_diff(current) <= 2,
        });

        let ensure = |app: &mut Self, key: (usize, Tier), budget: &mut i32| {
            if *budget <= 0 || app.textures.contains_key(&key) {
                return;
            }
            if let Some(buf) = app.cache.get_rgba(key) {
                let tex = ctx.load_texture(
                    format!("img{}-{:?}", key.0, key.1),
                    to_color_image(&buf),
                    egui::TextureOptions::LINEAR,
                );
                app.textures.insert(key, tex);
                *budget -= 1;
            }
        };
        // Current image may consume up to two uploads (browse + full).
        let mut budget = 2;
        ensure(self, (current, Tier::Browse), &mut budget);
        ensure(self, (current, Tier::Full), &mut budget);
        // One neighbor pre-upload per frame, forward first.
        let mut neighbor_budget = 1;
        let fwd = self.direction >= 0;
        let order: [isize; 4] = if fwd { [1, 2, -1, -2] } else { [-1, -2, 1, 2] };
        for d in order {
            let i = current as isize + d;
            if i >= 0 && (i as usize) < self.entries.len() {
                ensure(self, (i as usize, Tier::Browse), &mut neighbor_budget);
            }
        }
    }

    fn handle_keys(
        &mut self,
        ctx: &egui::Context,
        loupe_rect: egui::Rect,
        img_size: Option<egui::Vec2>,
    ) {
        let (right, left, shift, home, end, toggle, rating) = ctx.input(|i| {
            let rating = [
                egui::Key::Num0,
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
            ]
            .iter()
            .position(|k| i.key_pressed(*k))
            .map(|n| n as u8);
            (
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowLeft),
                i.modifiers.shift,
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
                i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Z),
                rating,
            )
        });
        if let Some(r) = rating {
            self.set_rating(r);
        }
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
            self.replan();
        }
    }

    /// Best displayable texture for the current image.
    fn best_current(&self) -> Option<(Tier, &egui::TextureHandle)> {
        self.textures
            .get(&(self.current, Tier::Full))
            .map(|t| (Tier::Full, t))
            .or_else(|| {
                self.textures
                    .get(&(self.current, Tier::Browse))
                    .map(|t| (Tier::Browse, t))
            })
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        self.manage_textures(&ctx);

        // Filmstrip.
        let current = self.current;
        let mut clicked: Option<usize> = None;
        egui::Panel::bottom("filmstrip")
            .exact_size(112.0)
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            for i in 0..self.entries.len() {
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

        // Status bar.
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
                    let stats = self.cache.stats();
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  |  rgba {}M  jpeg {}M",
                            self.status,
                            stats.rgba_bytes / (1024 * 1024),
                            stats.jpeg_bytes / (1024 * 1024),
                        ))
                        .weak(),
                    );
                });
            });
        });

        // Loupe.
        let loupe_rect = ui.available_rect_before_wrap();
        let mut img_size = None;
        match self.best_current() {
            Some((tier, tex)) => {
                let size = tex.size_vec2();
                img_size = Some(size);
                let tex = tex.clone();
                if let Some(t0) = self.nav_started.take() {
                    self.status = format!("{tier:?} in {:.0?}", t0.elapsed());
                }
                let response = loupe::show(ui, &tex, size, &mut self.zoom);
                if let Some(pos) = response.double_clicked_at {
                    loupe::toggle_100(&mut self.zoom, loupe_rect, size, pos);
                    self.replan();
                }
            }
            None => {
                // Waiting on first develop: thumb placeholder or spinner.
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

        // Rating overlay, bottom-left of the loupe.
        let rating = self.ratings.get(&self.current).copied().unwrap_or(0);
        let stars: String = (0..5).map(|i| if i < rating { '★' } else { '☆' }).collect();
        egui::Area::new(egui::Id::new("rating-overlay"))
            .fixed_pos(loupe_rect.left_bottom() + egui::vec2(12.0, -40.0))
            .show(&ctx, |ui| {
                ui.label(
                    egui::RichText::new(stars)
                        .size(24.0)
                        .color(if rating > 0 {
                            egui::Color32::GOLD
                        } else {
                            egui::Color32::from_white_alpha(48)
                        })
                        .background_color(egui::Color32::from_black_alpha(120)),
                );
            });

        self.handle_keys(&ctx, loupe_rect, img_size);
    }
}

fn to_color_image(buf: &PixelBuf) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([buf.width as usize, buf.height as usize], &buf.rgba)
}
