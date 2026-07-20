//! The app shell: draws from the RamCache, dispatches navigation to the
//! engine, and owns every texture (workers never touch UI types).
//!
//! Modes: Loupe (filmstrip + big image) and Grid. A min-rating filter
//! produces the visible sequence; the engine's prefetch wave follows it.
//! Refiltering applies lazily on navigation so rating an image below the
//! threshold doesn't yank it out from under the cursor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

use crate::config::{Action, Config, ScrollMode};
use crate::loupe::{self, Zoom};
use crate::settings::SettingsState;

const THUMB_BUDGET: u64 = 384 * 1024 * 1024;

pub fn run(dir: &Path, select: Option<&Path>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1500.0, 950.0]),
        ..Default::default()
    };
    let dir = dir.to_owned();
    let select = select.map(Path::to_owned);
    eframe::run_native(
        "viewr",
        options,
        Box::new(move |cc| {
            crate::color::pin_srgb_colorspace(cc);
            let mut app = App::empty(cc);
            app.open_folder(&dir, select.as_deref())
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Loupe,
    Grid,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Filter {
    /// 0 = off; N = show only rating ≥ N.
    min_rating: u8,
    unrated_only: bool,
}

impl Filter {
    fn passes(&self, rating: u8) -> bool {
        if self.unrated_only {
            return rating == 0;
        }
        rating >= self.min_rating
    }
    fn active(&self) -> bool {
        self.min_rating > 0 || self.unrated_only
    }
}

/// Per-folder session state, rebuilt by open_folder.
struct Session {
    dir: PathBuf,
    entries: Arc<Vec<FolderEntry>>,
    engine: Engine,
    events: Receiver<Event>,
    cache: Arc<RamCache>,
    library: Library,
    ratings: HashMap<usize, u8>,
    metas: HashMap<usize, FileMeta>,
    thumbs: HashMap<usize, egui::TextureHandle>,
    textures: HashMap<(usize, Tier), egui::TextureHandle>,
}

pub struct App {
    ctx: egui::Context,
    config: Config,
    settings: SettingsState,
    session: Option<Session>,

    current: usize,
    direction: i8,
    zoom: Zoom,
    mode: Mode,
    filter: Filter,
    visible: Vec<usize>,
    fullscreen: bool,
    show_metadata: bool,

    nav_started: Option<Instant>,
    /// Last known full-res logical size; carries the zoom framing onto
    /// thumb placeholders before the new image's develop lands.
    last_logical: Option<egui::Vec2>,
    status: String,
    scroll_to_current: bool,
}

impl App {
    fn empty(cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            ctx: cc.egui_ctx.clone(),
            config: Config::load(),
            settings: SettingsState::default(),
            session: None,
            current: 0,
            direction: 1,
            zoom: Zoom::Fit,
            mode: Mode::Loupe,
            filter: Filter::default(),
            visible: Vec::new(),
            fullscreen: false,
            show_metadata: false,
            nav_started: None,
            last_logical: None,
            status: String::new(),
            scroll_to_current: true,
        }
    }

    fn open_folder(&mut self, dir: &Path, select: Option<&Path>) -> Result<()> {
        let entries = scan(dir)?;
        if entries.is_empty() {
            return Err(anyhow!("no raw files found in {}", dir.display()));
        }
        let start = select
            .and_then(|f| entries.iter().position(|e| e.path == f))
            .unwrap_or(0);
        let entries = Arc::new(entries);
        let ram_bytes = (self.config.ram_gb as f64 * 1e9) as u64;
        let cache = Arc::new(RamCache::new(
            THUMB_BUDGET,
            ram_bytes * 2 / 3,
            ram_bytes / 3,
        ));
        let disk = DiskCache::open_default((self.config.disk_gb as f64 * 1e9) as u64);
        let ctx = self.ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let (engine, events) = Engine::new((*entries).clone(), start, cache.clone(), disk, notify);

        let db = default_db_path().and_then(|p| Db::open(&p).ok());
        let ratings = load_ratings(&entries, db.as_ref());
        let library = Library::start();

        self.session = Some(Session {
            dir: dir.to_owned(),
            entries,
            engine,
            events,
            cache,
            library,
            ratings,
            metas: HashMap::new(),
            thumbs: HashMap::new(),
            textures: HashMap::new(),
        });
        self.current = start;
        self.direction = 1;
        self.zoom = Zoom::Fit;
        self.filter = Filter::default();
        self.nav_started = Some(Instant::now());
        self.scroll_to_current = true;
        self.apply_filter();
        self.replan();
        Ok(())
    }

    fn pick_folder(&mut self) {
        let start_dir = self.session.as_ref().map(|s| s.dir.clone());
        let mut dialog = rfd::FileDialog::new();
        if let Some(d) = start_dir {
            dialog = dialog.set_directory(d);
        }
        if let Some(dir) = dialog.pick_folder()
            && let Err(e) = self.open_folder(&dir, None)
        {
            self.status = format!("open failed: {e}");
        }
    }

    /// Rebuild the visible sequence from the filter. Falls back to "all"
    /// when nothing matches so navigation never dead-ends.
    fn apply_filter(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let all = || (0..session.entries.len()).collect::<Vec<_>>();
        self.visible = if self.filter.active() {
            let v: Vec<usize> = (0..session.entries.len())
                .filter(|i| {
                    self.filter
                        .passes(session.ratings.get(i).copied().unwrap_or(0))
                })
                .collect();
            if v.is_empty() { all() } else { v }
        } else {
            all()
        };
        session.engine.set_sequence(self.visible.clone());
    }

    fn replan(&self) {
        if let Some(session) = &self.session {
            session.engine.navigate(NavState {
                current: self.current,
                direction: self.direction,
                zoomed: !matches!(self.zoom, Zoom::Fit),
            });
        }
    }

    /// Position of `current` within the visible sequence (nearest if the
    /// current image was filtered out).
    fn visible_pos(&self) -> usize {
        self.visible
            .iter()
            .position(|&i| i == self.current)
            .unwrap_or_else(|| {
                self.visible
                    .iter()
                    .position(|&i| i > self.current)
                    .unwrap_or(self.visible.len().saturating_sub(1))
            })
    }

    fn navigate(&mut self, delta: isize) {
        self.apply_filter(); // lazy refilter happens at nav time
        if self.visible.is_empty() {
            return;
        }
        let pos = self.visible_pos() as isize;
        let on_visible = self.visible.get(pos as usize) == Some(&self.current);
        // Stepping off a filtered-out image: land on the neighbor itself.
        let adjust = if !on_visible && delta > 0 { -1 } else { 0 };
        let next = (pos + delta + adjust).clamp(0, self.visible.len() as isize - 1) as usize;
        if delta != 0 {
            self.direction = if delta < 0 { -1 } else { 1 };
        }
        self.select(self.visible[next]);
    }

    fn select(&mut self, index: usize) {
        if index == self.current {
            return;
        }
        self.direction = if index < self.current { -1 } else { 1 };
        self.current = index;
        // Zoom state is intentionally retained: culling a burst at 100%
        // keeps the same framing (eye, detail) across images.
        self.scroll_to_current = true;
        self.nav_started = Some(Instant::now());
        if let Some(s) = &self.session {
            s.library.flush();
        }
        self.replan();
    }

    fn set_rating(&mut self, rating: u8) {
        let index = self.current;
        let Some(session) = &mut self.session else {
            return;
        };
        if rating == 0 {
            session.ratings.remove(&index);
        } else {
            session.ratings.insert(index, rating);
        }
        session.library.set_rating(&session.entries[index], rating);
    }

    fn drain_events(&mut self) {
        let ctx = self.ctx.clone();
        let current = self.current;
        let Some(session) = &mut self.session else {
            return;
        };
        let mut replan = false;
        while let Ok(event) = session.events.try_recv() {
            match event {
                Event::ThumbReady { index, meta } => {
                    if let Some(buf) = session.cache.get_rgba((index, Tier::Thumb)) {
                        session.thumbs.insert(
                            index,
                            ctx.load_texture(
                                format!("thumb{index}"),
                                to_color_image(&buf),
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                    }
                    if let Some(embedded) = meta.rating
                        && embedded > 0
                        && !session.ratings.contains_key(&index)
                    {
                        session.ratings.insert(index, embedded.min(5) as u8);
                    }
                    session.metas.insert(index, *meta);
                }
                Event::ImageReady { .. } => replan = true,
                Event::ImageFailed { index, tier, error } => {
                    if index == current && tier != Tier::Thumb {
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

    /// Upload policy: current image first (Browse then Full), then one
    /// neighbor browse per frame; prune outside the keep window.
    fn manage_textures(&mut self) {
        let ctx = self.ctx.clone();
        let current = self.current;
        // Keep-window over the visible sequence.
        let pos = self.visible_pos();
        let near: Vec<usize> = (-2isize..=2)
            .filter_map(|d| {
                let p = pos as isize + d;
                (p >= 0)
                    .then(|| self.visible.get(p as usize).copied())
                    .flatten()
            })
            .collect();
        let Some(session) = &mut self.session else {
            return;
        };
        session.textures.retain(|(i, tier), _| match tier {
            Tier::Full => *i == current,
            _ => near.contains(i),
        });

        let mut upload = |key: (usize, Tier), budget: &mut i32| {
            if *budget <= 0 || session.textures.contains_key(&key) {
                return;
            }
            if let Some(buf) = session.cache.get_rgba(key) {
                let tex = ctx.load_texture(
                    format!("img{}-{:?}", key.0, key.1),
                    to_color_image(&buf),
                    egui::TextureOptions::LINEAR,
                );
                session.textures.insert(key, tex);
                *budget -= 1;
            }
        };
        let mut budget = 2;
        upload((current, Tier::Browse), &mut budget);
        upload((current, Tier::Full), &mut budget);
        let mut neighbor_budget = 1;
        for &i in near.iter().filter(|&&i| i != current) {
            upload((i, Tier::Browse), &mut neighbor_budget);
        }
    }

    fn handle_keys(&mut self, loupe_rect: egui::Rect, img_size: Option<egui::Vec2>) {
        if self.settings.capturing() {
            return; // keystrokes are being captured as new binds
        }
        let ctx = self.ctx.clone();
        struct Keys {
            right: bool,
            left: bool,
            up: bool,
            down: bool,
            shift: bool,
            home: bool,
            end: bool,
            toggle_zoom: bool,
            grid: bool,
            info: bool,
            fullscreen: bool,
            open: bool,
            prefs: bool,
            rating: Option<u8>,
        }
        let config = &self.config;
        let in_grid = self.mode == Mode::Grid;
        let k = ctx.input(|i| Keys {
            right: config.pressed(i, Action::Next),
            left: config.pressed(i, Action::Prev),
            up: i.key_pressed(egui::Key::ArrowUp),
            down: i.key_pressed(egui::Key::ArrowDown),
            shift: i.modifiers.shift,
            home: config.pressed(i, Action::First),
            end: config.pressed(i, Action::Last),
            toggle_zoom: config.pressed(i, Action::ToggleZoom),
            grid: config.pressed(i, Action::Grid) || (in_grid && i.key_pressed(egui::Key::Enter)),
            info: config.pressed(i, Action::Metadata),
            fullscreen: config.pressed(i, Action::Fullscreen),
            open: config.pressed(i, Action::OpenFolder),
            prefs: config.pressed(i, Action::Preferences),
            rating: config.pressed_rating(i),
        });
        if k.prefs {
            self.settings.open = !self.settings.open;
        }

        if k.open {
            self.pick_folder();
            return;
        }
        if let Some(r) = k.rating {
            self.set_rating(r);
        }
        let step = if k.shift { 10 } else { 1 };
        let grid_cols = self.grid_cols() as isize;
        if k.right {
            self.navigate(step);
        }
        if k.left {
            self.navigate(-step);
        }
        if self.mode == Mode::Grid {
            if k.down {
                self.navigate(grid_cols);
            }
            if k.up {
                self.navigate(-grid_cols);
            }
        }
        if k.home {
            self.apply_filter();
            if let Some(&first) = self.visible.first() {
                self.select(first);
            }
        }
        if k.end {
            self.apply_filter();
            if let Some(&last) = self.visible.last() {
                self.select(last);
            }
        }
        if k.grid {
            self.mode = match self.mode {
                Mode::Loupe => Mode::Grid,
                Mode::Grid => Mode::Loupe,
            };
            self.scroll_to_current = true;
        }
        if k.info {
            self.show_metadata = !self.show_metadata;
        }
        if k.fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if k.toggle_zoom
            && self.mode == Mode::Loupe
            && let Some(size) = img_size
        {
            let anchor = ctx
                .pointer_hover_pos()
                .filter(|p| loupe_rect.contains(*p))
                .unwrap_or_else(|| loupe_rect.center());
            loupe::toggle_100(&mut self.zoom, loupe_rect, size, anchor);
            self.replan();
        }
    }

    /// Subtle border on the loupe image indicating the displayed cache
    /// tier: green = full-res develop, amber = browse, red = thumbnail
    /// stand-in (None = thumb).
    fn tier_border(&self, ui: &egui::Ui, draw_rect: egui::Rect, tier: Option<Tier>) {
        if !self.config.tier_border {
            return;
        }
        let color = match tier {
            Some(Tier::Full) => egui::Color32::from_rgba_unmultiplied(70, 200, 110, 110),
            Some(_) => egui::Color32::from_rgba_unmultiplied(240, 180, 60, 110),
            None => egui::Color32::from_rgba_unmultiplied(240, 90, 70, 130),
        };
        ui.painter().rect_stroke(
            draw_rect.shrink(1.0),
            0.0,
            egui::Stroke::new(2.0, color),
            egui::StrokeKind::Outside,
        );
    }

    fn grid_cols(&self) -> usize {
        let width = self.ctx.content_rect().width();
        ((width / 216.0).floor() as usize).max(2)
    }

    fn rating_of(&self, index: usize) -> u8 {
        self.session
            .as_ref()
            .and_then(|s| s.ratings.get(&index).copied())
            .unwrap_or(0)
    }
}

fn stars(rating: u8) -> String {
    (0..5).map(|i| if i < rating { '★' } else { '☆' }).collect()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = self.ctx.clone();
        self.settings.maybe_capture(&ctx, &mut self.config);
        self.settings.show(&ctx, &mut self.config);
        self.drain_events();
        self.manage_textures();
        if self.session.is_none() {
            ui.centered_and_justified(|u| {
                u.label("Open a folder of raws with Cmd+O");
            });
            self.handle_keys(ui.max_rect(), None);
            return;
        }

        self.top_bar(ui);
        if self.show_metadata {
            self.metadata_panel(ui);
        }
        match self.mode {
            Mode::Loupe => self.loupe_mode(ui),
            Mode::Grid => self.grid_mode(ui),
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let mut filter_changed = false;
        let session_len = self.session.as_ref().map_or(0, |s| s.entries.len());
        let file_name = self
            .session
            .as_ref()
            .map(|s| s.entries[self.current].file_name.clone())
            .unwrap_or_default();
        egui::Panel::top("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{}/{}  {}",
                    self.visible_pos() + 1,
                    self.visible.len(),
                    file_name,
                ));
                if self.visible.len() != session_len {
                    ui.label(egui::RichText::new(format!("(of {session_len})")).weak());
                }
                ui.separator();

                // Filter: ≥N stars + unrated-only.
                ui.label("filter:");
                for n in 1..=5u8 {
                    let active = self.filter.min_rating >= n && !self.filter.unrated_only;
                    let star = if active { '★' } else { '☆' };
                    if ui
                        .add(egui::Button::new(star.to_string()).frame(false))
                        .clicked()
                    {
                        self.filter.unrated_only = false;
                        self.filter.min_rating = if self.filter.min_rating == n { 0 } else { n };
                        filter_changed = true;
                    }
                }
                if ui
                    .selectable_label(self.filter.unrated_only, "unrated")
                    .clicked()
                {
                    self.filter.unrated_only = !self.filter.unrated_only;
                    self.filter.min_rating = 0;
                    filter_changed = true;
                }
                ui.separator();

                if let Some(meta) = self
                    .session
                    .as_ref()
                    .and_then(|s| s.metas.get(&self.current))
                {
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
                    ui.label(parts.join("  "));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("⚙")
                        .on_hover_text("Preferences (Cmd+,)")
                        .clicked()
                    {
                        self.settings.open = !self.settings.open;
                    }
                    if let Some(s) = &self.session {
                        let st = s.cache.stats();
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  |  rgba {}M  jpeg {}M",
                                self.status,
                                st.rgba_bytes / (1024 * 1024),
                                st.jpeg_bytes / (1024 * 1024),
                            ))
                            .weak(),
                        );
                    }
                });
            });
        });
        if filter_changed {
            self.apply_filter();
            // If the current image fell out of view, jump to the nearest.
            if !self.visible.contains(&self.current)
                && let Some(&target) = self.visible.get(self.visible_pos())
            {
                self.select(target);
            }
            self.scroll_to_current = true;
            self.replan();
        }
    }

    fn metadata_panel(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &self.session else {
            return;
        };
        let entry = &session.entries[self.current];
        let meta = session.metas.get(&self.current);
        let rating = self.rating_of(self.current);
        egui::Panel::right("metadata")
            .exact_size(260.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading(&entry.file_name);
                ui.label(stars(rating));
                ui.separator();
                if let Some(m) = meta {
                    ui.label(&m.camera);
                    if let Some(lens) = &m.lens {
                        ui.label(lens);
                    }
                    ui.add_space(6.0);
                    egui::Grid::new("exif").num_columns(2).show(ui, |ui| {
                        if let Some(iso) = m.iso {
                            ui.label("ISO");
                            ui.label(iso.to_string());
                            ui.end_row();
                        }
                        if let Some(s) = &m.shutter {
                            ui.label("Shutter");
                            ui.label(s);
                            ui.end_row();
                        }
                        if let Some(a) = &m.aperture {
                            ui.label("Aperture");
                            ui.label(a);
                            ui.end_row();
                        }
                        if let Some(f) = m.focal_mm {
                            ui.label("Focal");
                            ui.label(format!("{f:.0}mm"));
                            ui.end_row();
                        }
                        if let Some(t) = &m.taken {
                            ui.label("Taken");
                            ui.label(t);
                            ui.end_row();
                        }
                        ui.label("Size");
                        ui.label(format!("{:.1} MB", entry.size as f64 / 1e6));
                        ui.end_row();
                    });
                } else {
                    ui.spinner();
                }
            });
    }

    fn loupe_mode(&mut self, ui: &mut egui::Ui) {
        // Filmstrip over the visible sequence.
        let mut clicked: Option<usize> = None;
        let current = self.current;
        let scroll_to = self.scroll_to_current;
        if let Some(session) = &self.session {
            egui::Panel::bottom("filmstrip")
                .exact_size(112.0)
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal_centered(|ui| {
                                for &i in &self.visible {
                                    let selected = i == current;
                                    let rating = session.ratings.get(&i).copied().unwrap_or(0);
                                    let response = match session.thumbs.get(&i) {
                                        Some(tex) => {
                                            let size = tex.size_vec2();
                                            let h = 84.0;
                                            let w = (size.x / size.y * h).clamp(30.0, 170.0);
                                            ui.vertical(|ui| {
                                                let r = ui.add(
                                                    egui::Button::image(egui::Image::new((
                                                        tex.id(),
                                                        vec2(w, h),
                                                    )))
                                                    .selected(selected),
                                                );
                                                if rating > 0 {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "★".repeat(rating as usize),
                                                        )
                                                        .size(10.0)
                                                        .color(egui::Color32::GOLD),
                                                    );
                                                }
                                                r
                                            })
                                            .inner
                                        }
                                        None => ui.add_sized(
                                            vec2(112.0, 84.0),
                                            egui::Button::new(egui::RichText::new("…").weak())
                                                .selected(selected),
                                        ),
                                    };
                                    if response.clicked() {
                                        clicked = Some(i);
                                    }
                                    if selected && scroll_to {
                                        response.scroll_to_me(Some(egui::Align::Center));
                                    }
                                }
                            });
                        });
                });
        }
        self.scroll_to_current = false;
        if let Some(i) = clicked {
            self.select(i);
        }

        // Loupe. The zoom state lives in full-res "logical" space so the
        // same framing holds no matter which tier backs the texture:
        // full = exact, browse = half-res (×2), thumb = stand-in drawn at
        // the retained framing (blurry→sharp in place, no flash-to-fit).
        let loupe_rect = ui.available_rect_before_wrap();
        let mut img_size = None;
        let best = self.session.as_ref().and_then(|s| {
            s.textures
                .get(&(self.current, Tier::Full))
                .map(|t| (Tier::Full, t.clone(), t.size_vec2()))
                .or_else(|| {
                    s.textures
                        .get(&(self.current, Tier::Browse))
                        .map(|t| (Tier::Browse, t.clone(), t.size_vec2() * 2.0))
                })
        });
        let mut standin = false; // zoomed onto a lower tier than Full
        match best {
            Some((tier, tex, logical)) => {
                img_size = Some(logical);
                self.last_logical = Some(logical);
                if let Some(t0) = self.nav_started.take() {
                    self.status = format!("{tier:?} in {:.0?}", t0.elapsed());
                }
                standin = tier != Tier::Full && !matches!(self.zoom, Zoom::Fit);
                let scroll_zooms = self.config.scroll == ScrollMode::Zoom;
                let response = loupe::show(ui, &tex, logical, &mut self.zoom, scroll_zooms);
                self.tier_border(ui, response.draw_rect, Some(tier));
                if let Some(pos) = response.double_clicked_at {
                    loupe::toggle_100(&mut self.zoom, loupe_rect, logical, pos);
                    self.replan();
                }
            }
            None => {
                let thumb = self
                    .session
                    .as_ref()
                    .and_then(|s| s.thumbs.get(&self.current).cloned());
                if let Some(tex) = thumb {
                    // Burst frames share dimensions, so the previous logical
                    // size keeps the framing; fall back to fit otherwise.
                    let logical = self.last_logical.unwrap_or_else(|| tex.size_vec2());
                    img_size = Some(logical);
                    standin = !matches!(self.zoom, Zoom::Fit);
                    let scroll_zooms = self.config.scroll == ScrollMode::Zoom;
                    let response = loupe::show(ui, &tex, logical, &mut self.zoom, scroll_zooms);
                    self.tier_border(ui, response.draw_rect, None);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            }
        }

        // Never let a low-res stand-in masquerade as full res while
        // judging focus: badge until the Full texture takes over.
        if standin {
            egui::Area::new(egui::Id::new("standin-badge"))
                .fixed_pos(loupe_rect.center_top() + egui::vec2(-70.0, 10.0))
                .show(&self.ctx.clone(), |ui| {
                    ui.label(
                        egui::RichText::new(" PREVIEW — loading full res ")
                            .size(13.0)
                            .color(egui::Color32::BLACK)
                            .background_color(egui::Color32::from_rgb(255, 200, 60)),
                    );
                });
        }

        // Rating overlay.
        let rating = self.rating_of(self.current);
        egui::Area::new(egui::Id::new("rating-overlay"))
            .fixed_pos(loupe_rect.left_bottom() + egui::vec2(12.0, -40.0))
            .show(&self.ctx.clone(), |ui| {
                ui.label(
                    egui::RichText::new(stars(rating))
                        .size(24.0)
                        .color(if rating > 0 {
                            egui::Color32::GOLD
                        } else {
                            egui::Color32::from_white_alpha(48)
                        })
                        .background_color(egui::Color32::from_black_alpha(120)),
                );
            });

        self.handle_keys(loupe_rect, img_size);
    }

    fn grid_mode(&mut self, ui: &mut egui::Ui) {
        let cols = self.grid_cols();
        let cell = vec2(200.0, 150.0);
        let mut clicked: Option<usize> = None;
        let mut open_loupe = false;
        let current = self.current;
        let scroll_to = self.scroll_to_current;
        let rect = ui.max_rect();
        if let Some(session) = &self.session {
            let rows = self.visible.len().div_ceil(cols);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_rows(ui, cell.y + 26.0, rows, |ui, range| {
                    for row in range {
                        ui.horizontal(|ui| {
                            for col in 0..cols {
                                let Some(&i) = self.visible.get(row * cols + col) else {
                                    break;
                                };
                                let selected = i == current;
                                let rating = session.ratings.get(&i).copied().unwrap_or(0);
                                ui.vertical(|ui| {
                                    ui.set_width(cell.x);
                                    let response = match session.thumbs.get(&i) {
                                        Some(tex) => {
                                            let size = tex.size_vec2();
                                            let scale =
                                                (cell.x / size.x).min(cell.y / size.y).min(1.0);
                                            ui.add(
                                                egui::Button::image(egui::Image::new((
                                                    tex.id(),
                                                    size * scale,
                                                )))
                                                .selected(selected),
                                            )
                                        }
                                        None => ui.add_sized(
                                            cell,
                                            egui::Button::new(egui::RichText::new("…").weak())
                                                .selected(selected),
                                        ),
                                    };
                                    ui.label(
                                        egui::RichText::new(if rating > 0 {
                                            "★".repeat(rating as usize)
                                        } else {
                                            String::new()
                                        })
                                        .size(11.0)
                                        .color(egui::Color32::GOLD),
                                    );
                                    if response.clicked() {
                                        clicked = Some(i);
                                    }
                                    if response.double_clicked() {
                                        clicked = Some(i);
                                        open_loupe = true;
                                    }
                                    if selected && scroll_to {
                                        response.scroll_to_me(Some(egui::Align::Center));
                                    }
                                });
                            }
                        });
                    }
                });
        }
        self.scroll_to_current = false;
        if let Some(i) = clicked {
            self.select(i);
        }
        if open_loupe {
            self.mode = Mode::Loupe;
        }
        self.handle_keys(rect, None);
    }
}

fn to_color_image(buf: &PixelBuf) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([buf.width as usize, buf.height as usize], &buf.rgba)
}
