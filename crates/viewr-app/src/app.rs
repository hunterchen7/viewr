//! The app shell: draws from the RamCache, dispatches navigation to the
//! engine, and owns every texture (workers never touch UI types).
//!
//! Modes: Loupe (filmstrip + big image) and Grid. A min-rating filter
//! produces the visible sequence; the engine's prefetch wave follows it.
//! Refiltering applies lazily on navigation so rating an image below the
//! threshold doesn't yank it out from under the cursor.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

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
use crate::filmstrip;
use crate::loupe::{self, Zoom};
use crate::settings::SettingsState;
use crate::texture_lru::ByteLru;

const THUMB_BUDGET: u64 = 384 * 1024 * 1024;
/// Logical RGBA bytes retained by thumbnail texture handles. Actual backend
/// allocation can be slightly higher, but remains proportional to this cap.
const THUMB_TEXTURE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const THUMB_UPLOADS_PER_FRAME: usize = 8;
const THUMB_REQUEST_POLL_AFTER: Duration = Duration::from_millis(16);
const THUMB_REQUEST_STALE_AFTER: Duration = Duration::from_millis(500);
const THUMB_FAILURE_RETRY_AFTER: Duration = Duration::from_secs(2);

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

/// Rebuild `visible` in source order. Returns whether that order is the
/// identity sequence understood by the engine.
fn rebuild_visible(
    visible: &mut Vec<usize>,
    len: usize,
    ratings: &HashMap<usize, u8>,
    filter: Filter,
) -> bool {
    visible.clear();
    if filter.active() {
        visible.extend((0..len).filter(|i| filter.passes(ratings.get(i).copied().unwrap_or(0))));
    }
    if visible.is_empty() {
        visible.extend(0..len);
    }
    visible.len() == len
}

/// Position of `current` in a sorted visible sequence, or the next visible
/// item when the current one has just been filtered out.
fn visible_position(visible: &[usize], current: usize) -> usize {
    match visible.binary_search(&current) {
        Ok(pos) => pos,
        Err(pos) => pos.min(visible.len().saturating_sub(1)),
    }
}

fn full_texture_work_enabled(zoom: Zoom) -> bool {
    !matches!(zoom, Zoom::Fit)
}

fn current_texture_candidates(current: usize, zoom: Zoom) -> impl Iterator<Item = (usize, Tier)> {
    let include_full = full_texture_work_enabled(zoom);
    [(current, Tier::Browse), (current, Tier::Full)]
        .into_iter()
        .filter(move |(_, tier)| include_full || *tier != Tier::Full)
}

fn main_texture_should_be_kept(
    index: usize,
    tier: Tier,
    current: usize,
    near: &[usize],
    zoom: Zoom,
) -> bool {
    match tier {
        Tier::Full => full_texture_work_enabled(zoom) && index == current,
        _ => near.contains(&index),
    }
}

fn install_metadata(
    ratings: &mut HashMap<usize, u8>,
    metas: &mut HashMap<usize, FileMeta>,
    index: usize,
    meta: FileMeta,
    filter: Filter,
) -> bool {
    let mut filter_dirty = false;
    if let Some(embedded) = meta.rating
        && embedded > 0
        && !ratings.contains_key(&index)
    {
        let rating = embedded.min(5) as u8;
        ratings.insert(index, rating);
        filter_dirty = filter.passes(0) != filter.passes(rating);
    }
    metas.insert(index, meta);
    filter_dirty
}

/// Store every explicit user choice, including zero. Absence means that no
/// higher-precedence rating source has been observed yet, so removing a zero
/// would let a delayed embedded-metadata event resurrect the camera rating.
fn install_user_rating(ratings: &mut HashMap<usize, u8>, index: usize, rating: u8) -> u8 {
    let old_rating = ratings.get(&index).copied().unwrap_or(0);
    ratings.insert(index, rating);
    old_rating
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
    thumbs: ByteLru<egui::TextureHandle>,
    /// Demand requests are bounded to the current viewport and time out so a
    /// publish/worker-finish race cannot leave an evicted thumbnail stuck.
    thumb_requests: HashMap<usize, Instant>,
    thumb_retry_after: HashMap<usize, Instant>,
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
    /// Ratings only affect the visible sequence lazily, on navigation.
    filter_dirty: bool,
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
            filter_dirty: true,
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
        // Resolve persisted ratings before decode workers can publish embedded
        // metadata, so startup precedence does not depend on worker timing.
        let db = default_db_path().and_then(|p| Db::open(&p).ok());
        let ratings = load_ratings(&entries, db.as_ref());
        let library = Library::start();

        let ctx = self.ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let (engine, events) = Engine::new(entries.clone(), start, cache.clone(), disk, notify);

        self.session = Some(Session {
            dir: dir.to_owned(),
            entries,
            engine,
            events,
            cache,
            library,
            ratings,
            metas: HashMap::new(),
            thumbs: ByteLru::new(THUMB_TEXTURE_BUDGET_BYTES),
            thumb_requests: HashMap::new(),
            thumb_retry_after: HashMap::new(),
            textures: HashMap::new(),
        });
        self.current = start;
        self.direction = 1;
        self.zoom = Zoom::Fit;
        self.filter = Filter::default();
        self.filter_dirty = true;
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
        if !self.filter_dirty {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let identity = rebuild_visible(
            &mut self.visible,
            session.entries.len(),
            &session.ratings,
            self.filter,
        );
        // The engine treats an empty sequence as the identity order, avoiding
        // another full-size allocation for the common unfiltered case.
        let engine_sequence = if identity {
            Vec::new()
        } else {
            self.visible.clone()
        };
        session.engine.set_sequence(engine_sequence);
        self.filter_dirty = false;
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
        visible_position(&self.visible, self.current)
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
        let old_rating = install_user_rating(&mut session.ratings, index, rating);
        session.library.set_rating(&session.entries[index], rating);
        if self.filter.passes(old_rating) != self.filter.passes(rating) {
            self.filter_dirty = true;
        }
    }

    fn drain_events(&mut self) {
        let current = self.current;
        let Some(session) = &mut self.session else {
            return;
        };
        let mut replan = false;
        let mut filter_dirty = false;
        while let Ok(event) = session.events.try_recv() {
            match event {
                Event::ThumbReady { index, meta } => {
                    // Pixels stay in the byte-bounded RAM ring until a visible
                    // viewport asks to upload them. If they were already evicted,
                    // the demand path below safely queues a replacement decode.
                    session.thumb_requests.remove(&index);
                    session.thumb_retry_after.remove(&index);
                    filter_dirty |= install_metadata(
                        &mut session.ratings,
                        &mut session.metas,
                        index,
                        *meta,
                        self.filter,
                    );
                }
                Event::MetadataReady { index, meta } => {
                    filter_dirty |= install_metadata(
                        &mut session.ratings,
                        &mut session.metas,
                        index,
                        *meta,
                        self.filter,
                    );
                }
                Event::ImageReady { .. } => replan = true,
                Event::ImageFailed { index, tier, error } => {
                    if tier == Tier::Thumb && session.thumb_requests.remove(&index).is_some() {
                        session
                            .thumb_retry_after
                            .insert(index, Instant::now() + THUMB_FAILURE_RETRY_AFTER);
                    } else if index == current && tier != Tier::Thumb {
                        self.status = format!("error: {error}");
                    } else {
                        eprintln!("job failed {index}/{tier:?}: {error}");
                    }
                }
                Event::MetadataFailed { index, error } => {
                    eprintln!("metadata failed {index}: {error}");
                }
            }
        }
        self.filter_dirty |= filter_dirty;
        if replan {
            self.replan();
        }
    }

    /// Upload only thumbnails demanded by the current viewport. The byte LRU
    /// drops old `TextureHandle`s (and therefore their GPU allocations), while
    /// the upload budget prevents a newly opened grid from monopolizing a frame.
    fn manage_thumbnail_textures(&mut self, demanded_indices: &[usize]) {
        let ctx = self.ctx.clone();
        let now = Instant::now();
        let demanded: HashSet<usize> = demanded_indices.iter().copied().collect();
        let Some(session) = &mut self.session else {
            return;
        };

        // Scrolling must not leave bookkeeping proportional to folder size.
        session
            .thumb_requests
            .retain(|index, _| demanded.contains(index));
        session
            .thumb_retry_after
            .retain(|index, _| demanded.contains(index));

        let mut upload_budget = THUMB_UPLOADS_PER_FRAME;
        let mut uploaded = false;
        let mut next_wakeup = None;
        let mut requested_thumbs = Vec::with_capacity(demanded_indices.len());
        let mut seen = HashSet::with_capacity(demanded_indices.len());
        for &index in demanded_indices {
            if index >= session.entries.len() || !seen.insert(index) {
                continue;
            }
            if session.thumbs.touch(index) {
                continue;
            }

            if let Some(buf) = session.cache.get_rgba((index, Tier::Thumb)) {
                if upload_budget > 0 {
                    let bytes = buf.byte_len();
                    let texture = ctx.load_texture(
                        format!("thumb{index}"),
                        to_color_image(&buf),
                        egui::TextureOptions::LINEAR,
                    );
                    if session.thumbs.insert(index, texture, bytes) {
                        upload_budget -= 1;
                        uploaded = true;
                        session.thumb_requests.remove(&index);
                        session.thumb_retry_after.remove(&index);
                    }
                }
                // The pixels are available and will be uploaded in a later
                // frame if this frame's upload allowance is exhausted.
                continue;
            }

            if let Some(retry_after) = session.thumb_retry_after.get(&index)
                && now < *retry_after
            {
                let delay = retry_after.saturating_duration_since(now);
                next_wakeup =
                    Some(next_wakeup.map_or(delay, |current: Duration| current.min(delay)));
                continue;
            }
            session.thumb_retry_after.remove(&index);
            requested_thumbs.push(index);

            let request_is_stale = match session.thumb_requests.get(&index) {
                Some(requested) => {
                    let elapsed = now.duration_since(*requested);
                    if elapsed < THUMB_REQUEST_STALE_AFTER {
                        let delay = THUMB_REQUEST_STALE_AFTER - elapsed;
                        next_wakeup =
                            Some(next_wakeup.map_or(delay, |current: Duration| current.min(delay)));
                        false
                    } else {
                        true
                    }
                }
                None => true,
            };
            if request_is_stale {
                session.thumb_requests.insert(index, now);
                // Poll once to close the narrow race where pixels appeared
                // between our cache probe and the worker claiming demand.
                next_wakeup = Some(next_wakeup.map_or(THUMB_REQUEST_POLL_AFTER, |current| {
                    current.min(THUMB_REQUEST_POLL_AFTER)
                }));
            }
        }
        let _ = session.engine.set_thumbnail_demand(&requested_thumbs);

        if uploaded {
            // Uploads happen after the widgets are painted, so schedule the
            // next frame that will actually display the new texture handles.
            ctx.request_repaint();
        }
        if let Some(delay) = next_wakeup {
            // Failure backoff and stale-request recovery must make progress
            // even when no worker event or user input produces another frame.
            ctx.request_repaint_after(delay);
        }
    }

    /// Upload policy: current Browse first, current Full only while zoomed,
    /// then one neighbor Browse per frame. Prune outside the keep window.
    fn manage_textures(&mut self) {
        let ctx = self.ctx.clone();
        let current = self.current;
        let zoom = self.zoom;
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
        session
            .textures
            .retain(|(i, tier), _| main_texture_should_be_kept(*i, *tier, current, &near, zoom));

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
        for key in current_texture_candidates(current, zoom) {
            upload(key, &mut budget);
        }
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

    /// Cache-tier stroke color for a thumbnail: green = full-res in RAM,
    /// amber = browse in RAM, dim blue = warm (JPEG ring, instant
    /// rehydrate), none = cold.
    fn tier_stroke(&self, session: &Session, index: usize) -> Option<egui::Color32> {
        if !self.config.tier_border {
            return None;
        }
        let cache = &session.cache;
        if cache.has_rgba((index, Tier::Full)) {
            Some(egui::Color32::from_rgba_unmultiplied(70, 200, 110, 150))
        } else if cache.has_rgba((index, Tier::Browse)) {
            Some(egui::Color32::from_rgba_unmultiplied(240, 180, 60, 150))
        } else if cache.has_jpeg((index, Tier::Browse)) || cache.has_jpeg((index, Tier::Full)) {
            Some(egui::Color32::from_rgba_unmultiplied(90, 140, 240, 110))
        } else {
            None
        }
    }

    fn grid_cols(&self) -> usize {
        let width = self.ctx.content_rect().width();
        ((width / (self.config.grid_cell + 16.0)).floor() as usize).max(2)
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

                // Filter dropdown (distinct from the rating display).
                let filter_label = |f: &Filter| -> String {
                    if f.unrated_only {
                        "unrated".into()
                    } else if f.min_rating == 0 {
                        "all".into()
                    } else {
                        format!("≥ {}", "★".repeat(f.min_rating as usize))
                    }
                };
                egui::ComboBox::from_id_salt("filter")
                    .selected_text(format!("filter: {}", filter_label(&self.filter)))
                    .show_ui(ui, |ui| {
                        let mut options: Vec<(String, Filter)> = vec![(
                            "all".into(),
                            Filter {
                                min_rating: 0,
                                unrated_only: false,
                            },
                        )];
                        for n in 1..=5u8 {
                            options.push((
                                format!("≥ {}", "★".repeat(n as usize)),
                                Filter {
                                    min_rating: n,
                                    unrated_only: false,
                                },
                            ));
                        }
                        options.push((
                            "unrated".into(),
                            Filter {
                                min_rating: 0,
                                unrated_only: true,
                            },
                        ));
                        for (label, value) in options {
                            if ui.selectable_label(self.filter == value, label).clicked() {
                                self.filter = value;
                                filter_changed = true;
                            }
                        }
                    });
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
            self.filter_dirty = true;
            self.apply_filter();
            // If the current image fell out of view, jump to the nearest.
            if self.visible.binary_search(&self.current).is_err()
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
        egui::Panel::right("metadata")
            .exact_size(260.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading(&entry.file_name);
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
        // Filmstrip over the visible sequence. Drag the divider to
        // resize; the height persists to viewr.toml.
        let mut clicked: Option<usize> = None;
        let current = self.current;
        let scroll_to = self.scroll_to_current;
        let mut strip_height = None;
        let mut demanded_thumbs = vec![current];
        if let Some(session) = &self.session {
            let inner = egui::Panel::bottom("filmstrip")
                .resizable(true)
                .default_size(self.config.filmstrip_height)
                .size_range(egui::Rangef::new(70.0, 320.0))
                .show(ui, |ui| {
                    let thumb_h = (ui.available_height() - 24.0).clamp(46.0, 290.0);
                    // Every column has a stable width so the strip can create only
                    // the widgets intersecting the viewport. Previously this loop
                    // built one widget tree per file even when almost all were clipped.
                    let cell = vec2(thumb_h * 1.4, thumb_h + 18.0);
                    let spacing = ui.spacing().item_spacing.x;
                    let visible_pos = visible_position(&self.visible, current);
                    let mut strip = egui::ScrollArea::horizontal()
                        .id_salt("filmstrip")
                        .auto_shrink([false, false]);
                    if scroll_to {
                        strip = strip.horizontal_scroll_offset(filmstrip::centered_scroll_offset(
                            self.visible.len(),
                            visible_pos,
                            cell.x,
                            spacing,
                            ui.available_width(),
                        ));
                    }
                    strip.show_viewport(ui, |ui, viewport| {
                        filmstrip::show_columns(
                            ui,
                            viewport,
                            self.visible.len(),
                            cell,
                            filmstrip::OVERSCAN_COLUMNS,
                            |columns_ui, range| {
                                for visible_pos in range {
                                    let i = self.visible[visible_pos];
                                    demanded_thumbs.push(i);
                                    let selected = i == current;
                                    let rating = session.ratings.get(&i).copied().unwrap_or(0);
                                    columns_ui.allocate_ui_with_layout(
                                        cell,
                                        egui::Layout::top_down(egui::Align::Center),
                                        |column_ui| {
                                            let response = match session.thumbs.get(&i) {
                                                Some(tex) => {
                                                    let size = tex.size_vec2();
                                                    let padding =
                                                        column_ui.spacing().button_padding * 2.0;
                                                    let inner = (vec2(cell.x, thumb_h) - padding)
                                                        .max(vec2(1.0, 1.0));
                                                    let scale =
                                                        (inner.x / size.x).min(inner.y / size.y);
                                                    column_ui.add_sized(
                                                        vec2(cell.x, thumb_h),
                                                        // `Button::image` caps image atoms at the
                                                        // font height in egui 0.35. A regular button
                                                        // intentionally preserves this exact size.
                                                        egui::Button::new(
                                                            egui::Image::new((tex.id(), size))
                                                                .fit_to_exact_size(size * scale),
                                                        )
                                                        .selected(selected),
                                                    )
                                                }
                                                None => column_ui.add_sized(
                                                    vec2(cell.x, thumb_h),
                                                    egui::Button::new(
                                                        egui::RichText::new("…").weak(),
                                                    )
                                                    .selected(selected),
                                                ),
                                            };
                                            if let Some(color) = self.tier_stroke(session, i) {
                                                column_ui.painter().rect_stroke(
                                                    response.rect.expand(1.0),
                                                    3.0,
                                                    egui::Stroke::new(2.0, color),
                                                    egui::StrokeKind::Outside,
                                                );
                                            }
                                            if rating > 0 {
                                                column_ui.label(
                                                    egui::RichText::new(
                                                        "★".repeat(rating as usize),
                                                    )
                                                    .size(10.0)
                                                    .color(egui::Color32::GOLD),
                                                );
                                            }
                                            if response.clicked() {
                                                clicked = Some(i);
                                            }
                                        },
                                    );
                                }
                            },
                        );
                    });
                });
            strip_height = Some(inner.response.rect.height());
        }
        self.manage_thumbnail_textures(&demanded_thumbs);
        // Persist a divider-drag once the mouse is released.
        if let Some(h) = strip_height
            && (h - self.config.filmstrip_height).abs() > 1.0
            && !self.ctx.input(|i| i.pointer.any_down())
        {
            self.config.filmstrip_height = h;
            self.config.save();
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
                    loupe::show(ui, &tex, logical, &mut self.zoom, scroll_zooms);
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
        let cell = vec2(self.config.grid_cell, self.config.grid_cell * 0.75);
        let mut clicked: Option<usize> = None;
        let mut open_loupe = false;
        let current = self.current;
        let scroll_to = self.scroll_to_current;
        let rect = ui.max_rect();
        let mut demanded_thumbs = vec![current];
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
                                demanded_thumbs.push(i);
                                let selected = i == current;
                                let rating = session.ratings.get(&i).copied().unwrap_or(0);
                                let tier_stroke = self.tier_stroke(session, i);
                                ui.vertical(|ui| {
                                    ui.set_width(cell.x);
                                    let response = match session.thumbs.get(&i) {
                                        Some(tex) => {
                                            let size = tex.size_vec2();
                                            let padding = ui.spacing().button_padding * 2.0;
                                            let inner = (cell - padding).max(vec2(1.0, 1.0));
                                            let scale =
                                                (inner.x / size.x).min(inner.y / size.y).min(1.0);
                                            ui.add_sized(
                                                cell,
                                                egui::Button::new(
                                                    egui::Image::new((tex.id(), size))
                                                        .fit_to_exact_size(size * scale),
                                                )
                                                .selected(selected),
                                            )
                                        }
                                        None => ui.add_sized(
                                            cell,
                                            egui::Button::new(egui::RichText::new("…").weak())
                                                .selected(selected),
                                        ),
                                    };
                                    if let Some(color) = tier_stroke {
                                        ui.painter().rect_stroke(
                                            response.rect.expand(1.0),
                                            3.0,
                                            egui::Stroke::new(2.0, color),
                                            egui::StrokeKind::Outside,
                                        );
                                    }
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
        self.manage_thumbnail_textures(&demanded_thumbs);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_with_rating(rating: Option<u32>) -> FileMeta {
        FileMeta {
            rating,
            camera: "Test Camera".into(),
            ..FileMeta::default()
        }
    }

    #[test]
    fn metadata_only_events_preserve_rating_precedence_and_filter_updates() {
        let mut ratings = HashMap::from([(1, 5)]);
        let mut metas = HashMap::new();
        let filter = Filter {
            min_rating: 4,
            unrated_only: false,
        };

        assert!(install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(4)),
            filter,
        ));
        assert_eq!(ratings.get(&0), Some(&4));
        assert_eq!(
            metas.get(&0).map(|meta| meta.camera.as_str()),
            Some("Test Camera")
        );

        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            1,
            metadata_with_rating(Some(2)),
            filter,
        ));
        assert_eq!(ratings.get(&1), Some(&5), "persisted rating must win");
    }

    #[test]
    fn delayed_metadata_and_thumbnail_events_do_not_resurrect_a_cleared_rating() {
        let mut ratings = HashMap::from([(0, 5)]);
        let mut metas = HashMap::new();
        let filter = Filter {
            min_rating: 0,
            unrated_only: true,
        };

        assert_eq!(install_user_rating(&mut ratings, 0, 0), 5);
        assert_eq!(ratings.get(&0), Some(&0), "zero remains authoritative");

        // Models a MetadataReady event that was decoded before the user's
        // clear-rating command reached the sidecar.
        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(4)),
            filter,
        ));
        assert_eq!(ratings.get(&0), Some(&0));

        // Models the duplicate metadata carried by a later ThumbReady event.
        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(3)),
            filter,
        ));
        assert_eq!(ratings.get(&0), Some(&0));

        let mut visible = Vec::new();
        assert!(rebuild_visible(&mut visible, 1, &ratings, filter));
        assert_eq!(
            visible,
            [0],
            "explicit zero still passes the unrated filter"
        );
    }

    #[test]
    fn visible_position_finds_exact_and_nearest_items() {
        let visible = [1, 4, 8, 12];
        assert_eq!(visible_position(&visible, 4), 1);
        assert_eq!(visible_position(&visible, 0), 0);
        assert_eq!(visible_position(&visible, 5), 2);
        assert_eq!(visible_position(&visible, 99), 3);
        assert_eq!(visible_position(&[], 5), 0);
    }

    #[test]
    fn fit_texture_policy_omits_full_while_zoomed_preserves_current_full() {
        let near = [6, 7, 8];
        assert_eq!(
            current_texture_candidates(7, Zoom::Fit).collect::<Vec<_>>(),
            [(7, Tier::Browse)]
        );
        assert!(!main_texture_should_be_kept(
            7,
            Tier::Full,
            7,
            &near,
            Zoom::Fit
        ));

        let zoomed = Zoom::Anchored {
            scale: 1.0,
            center: vec2(0.5, 0.5),
        };
        assert_eq!(
            current_texture_candidates(7, zoomed).collect::<Vec<_>>(),
            [(7, Tier::Browse), (7, Tier::Full)]
        );
        assert!(main_texture_should_be_kept(7, Tier::Full, 7, &near, zoomed));
        assert!(!main_texture_should_be_kept(
            8,
            Tier::Full,
            7,
            &near,
            zoomed
        ));
        assert!(main_texture_should_be_kept(
            8,
            Tier::Browse,
            7,
            &near,
            Zoom::Fit
        ));
    }

    #[test]
    fn rebuild_visible_filters_in_source_order() {
        let ratings = HashMap::from([(0, 1), (2, 4), (3, 5), (5, 3)]);
        let mut visible = Vec::new();
        let identity = rebuild_visible(
            &mut visible,
            6,
            &ratings,
            Filter {
                min_rating: 4,
                unrated_only: false,
            },
        );
        assert_eq!(visible, [2, 3]);
        assert!(!identity);
    }

    #[test]
    fn rebuild_visible_falls_back_to_identity_when_filter_matches_nothing() {
        let ratings = HashMap::from([(1, 2)]);
        let mut visible = vec![99];
        let identity = rebuild_visible(
            &mut visible,
            3,
            &ratings,
            Filter {
                min_rating: 5,
                unrated_only: false,
            },
        );
        assert_eq!(visible, [0, 1, 2]);
        assert!(identity);
    }

    #[test]
    fn rebuild_visible_handles_unfiltered_and_unrated_views() {
        let ratings = HashMap::from([(1, 3)]);
        let mut visible = Vec::new();
        assert!(rebuild_visible(
            &mut visible,
            3,
            &ratings,
            Filter::default()
        ));
        assert_eq!(visible, [0, 1, 2]);

        let identity = rebuild_visible(
            &mut visible,
            3,
            &ratings,
            Filter {
                min_rating: 0,
                unrated_only: true,
            },
        );
        assert_eq!(visible, [0, 2]);
        assert!(!identity);
    }
}
