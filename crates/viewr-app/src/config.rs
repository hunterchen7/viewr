//! User configuration: input behavior, UI prefs, cache budgets, keybinds.
//!
//! Lives at `~/Library/Application Support/viewr/viewr.toml`. A
//! documented template is written on first run; absent file or absent
//! keys fall back to defaults, so partial configs are fine. The
//! Preferences window edits this in memory and saves a regenerated file.

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use serde::Deserialize;
use viewr_core::jobs::{CACHE_JPEG_QUALITY, MAX_CACHE_JPEG_QUALITY, MIN_CACHE_JPEG_QUALITY};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Next,
    Prev,
    First,
    Last,
    ToggleZoom,
    Grid,
    Metadata,
    Fullscreen,
    OpenFolder,
    Preferences,
    Rate(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScrollMode {
    /// Plain scroll pans the zoomed image; pinch or Ctrl/Cmd+scroll zooms.
    #[default]
    Pan,
    /// Plain scroll zooms (the old behavior).
    Zoom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TierIndicator {
    /// Colored outline around a thumbnail.
    Border,
    /// Compact delivery-style marks below a thumbnail.
    #[default]
    Marks,
    /// Do not show cache-tier state.
    Hidden,
}

impl TierIndicator {
    fn as_str(self) -> &'static str {
        match self {
            Self::Border => "border",
            Self::Marks => "marks",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub key: egui::Key,
    pub mods: egui::Modifiers,
}

impl Bind {
    /// cmd/ctrl/alt must match exactly; shift is ignored unless the bind
    /// requires it (so Shift+arrow can mean "step 10" on a plain bind).
    fn is_pressed(&self, input: &egui::InputState) -> bool {
        let m = input.modifiers;
        input.key_pressed(self.key)
            && m.command == self.mods.command
            && m.ctrl == self.mods.ctrl
            && m.alt == self.mods.alt
            && (!self.mods.shift || m.shift)
    }

    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.mods.command {
            s.push_str("Cmd+");
        }
        if self.mods.ctrl {
            s.push_str("Ctrl+");
        }
        if self.mods.alt {
            s.push_str("Alt+");
        }
        if self.mods.shift {
            s.push_str("Shift+");
        }
        s.push_str(self.key.name());
        s
    }
}

pub struct Config {
    pub scroll: ScrollMode,
    pub tier_indicator: TierIndicator,
    pub show_loading: bool,
    pub show_performance: bool,
    pub show_exposure: bool,
    /// Filmstrip panel height in px (drag the divider to change).
    pub filmstrip_height: f32,
    /// Grid cell width in px.
    pub grid_cell: f32,
    /// Total RAM cache budget in GB, including thumbnails. The remainder
    /// after the thumbnail allowance is split between RGBA and JPEG rings.
    /// Applies on the next folder open.
    pub ram_gb: f32,
    /// Disk cache budget in GB. Applies on the next folder open.
    pub disk_gb: f32,
    /// JPEG quality for Browse and Full cache objects. Applies on the next
    /// folder open.
    pub jpeg_quality: u8,
    binds: HashMap<Action, Vec<Bind>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    input: RawInput,
    ui: RawUi,
    cache: RawCache,
    binds: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawInput {
    scroll: ScrollMode,
}

#[derive(Deserialize)]
#[serde(default)]
struct RawUi {
    /// New three-state preference. `None` permits migration from the old
    /// `tier_border` boolean.
    tier_indicator: Option<TierIndicator>,
    /// Compatibility with configurations written before tier marks existed.
    tier_border: Option<bool>,
    show_loading: bool,
    show_performance: bool,
    show_exposure: bool,
    filmstrip_height: f32,
    grid_cell: f32,
}
impl Default for RawUi {
    fn default() -> Self {
        Self {
            tier_indicator: None,
            tier_border: None,
            show_loading: true,
            show_performance: true,
            show_exposure: true,
            filmstrip_height: 112.0,
            grid_cell: 200.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawCache {
    ram_gb: f32,
    disk_gb: f32,
    jpeg_quality: i16,
}
impl Default for RawCache {
    fn default() -> Self {
        Self {
            ram_gb: 4.5,
            disk_gb: 20.0,
            jpeg_quality: i16::from(CACHE_JPEG_QUALITY),
        }
    }
}

pub const ACTIONS: &[(&str, &str, Action)] = &[
    ("next", "Next image", Action::Next),
    ("prev", "Previous image", Action::Prev),
    ("first", "First image", Action::First),
    ("last", "Last image", Action::Last),
    ("toggle_zoom", "Toggle fit / 100%", Action::ToggleZoom),
    ("grid", "Grid view", Action::Grid),
    ("metadata", "Metadata panel", Action::Metadata),
    ("fullscreen", "Fullscreen", Action::Fullscreen),
    ("open_folder", "Open folder", Action::OpenFolder),
    ("preferences", "Preferences", Action::Preferences),
    ("rate_0", "Clear rating", Action::Rate(0)),
    ("rate_1", "Rate ★", Action::Rate(1)),
    ("rate_2", "Rate ★★", Action::Rate(2)),
    ("rate_3", "Rate ★★★", Action::Rate(3)),
    ("rate_4", "Rate ★★★★", Action::Rate(4)),
    ("rate_5", "Rate ★★★★★", Action::Rate(5)),
];

fn default_binds(action: Action) -> &'static [&'static str] {
    match action {
        Action::Next => &["ArrowRight"],
        Action::Prev => &["ArrowLeft"],
        Action::First => &["Home"],
        Action::Last => &["End"],
        Action::ToggleZoom => &["Space", "Z"],
        Action::Grid => &["G"],
        Action::Metadata => &["I"],
        Action::Fullscreen => &["F"],
        Action::OpenFolder => &["Cmd+O"],
        Action::Preferences => &["Cmd+Comma"],
        Action::Rate(n) => match n {
            0 => &["0"],
            1 => &["1"],
            2 => &["2"],
            3 => &["3"],
            4 => &["4"],
            _ => &["5"],
        },
    }
}

/// "Cmd+Shift+O" → Bind. Modifiers: Cmd, Ctrl, Alt, Shift.
fn parse_bind(spec: &str) -> Option<Bind> {
    let mut mods = egui::Modifiers::NONE;
    let mut key = None;
    for part in spec.split('+') {
        match part.trim() {
            "Cmd" | "Command" | "Super" => mods.command = true,
            "Ctrl" | "Control" => mods.ctrl = true,
            "Alt" | "Option" => mods.alt = true,
            "Shift" => mods.shift = true,
            name => key = parse_key(name),
        }
    }
    key.map(|key| Bind { key, mods })
}

fn parse_key(name: &str) -> Option<egui::Key> {
    egui::Key::from_name(name).or(match name {
        "0" => Some(egui::Key::Num0),
        "1" => Some(egui::Key::Num1),
        "2" => Some(egui::Key::Num2),
        "3" => Some(egui::Key::Num3),
        "4" => Some(egui::Key::Num4),
        "5" => Some(egui::Key::Num5),
        "6" => Some(egui::Key::Num6),
        "7" => Some(egui::Key::Num7),
        "8" => Some(egui::Key::Num8),
        "9" => Some(egui::Key::Num9),
        "," | "Comma" => Some(egui::Key::Comma),
        _ => None,
    })
}

impl Config {
    pub fn load() -> Self {
        let raw = config_path()
            .and_then(|p| match std::fs::read_to_string(&p) {
                Ok(text) => toml::from_str::<RawConfig>(&text)
                    .map_err(|e| eprintln!("viewr.toml: {e}; using defaults"))
                    .ok(),
                Err(_) => {
                    let _ = std::fs::write(&p, TEMPLATE);
                    None
                }
            })
            .unwrap_or_default();
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Self {
        let default_ui = RawUi::default();
        let default_cache = RawCache::default();
        let mut binds = HashMap::new();
        for &(name, _, action) in ACTIONS {
            let specs: Vec<String> = raw.binds.get(name).cloned().unwrap_or_else(|| {
                default_binds(action)
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
            let parsed: Vec<Bind> = specs
                .iter()
                .filter_map(|s| {
                    let bind = parse_bind(s);
                    if bind.is_none() {
                        eprintln!("viewr.toml: unknown bind {s:?} for {name}");
                    }
                    bind
                })
                .collect();
            binds.insert(action, parsed);
        }
        let tier_indicator = raw.ui.tier_indicator.unwrap_or(match raw.ui.tier_border {
            Some(false) => TierIndicator::Hidden,
            Some(true) | None => TierIndicator::Marks,
        });
        Self {
            scroll: raw.input.scroll,
            tier_indicator,
            show_loading: raw.ui.show_loading,
            show_performance: raw.ui.show_performance,
            show_exposure: raw.ui.show_exposure,
            filmstrip_height: finite_clamp(
                raw.ui.filmstrip_height,
                default_ui.filmstrip_height,
                70.0,
                320.0,
            ),
            grid_cell: finite_clamp(raw.ui.grid_cell, default_ui.grid_cell, 120.0, 400.0),
            ram_gb: finite_clamp(raw.cache.ram_gb, default_cache.ram_gb, 1.0, 20.0),
            disk_gb: finite_clamp(raw.cache.disk_gb, default_cache.disk_gb, 1.0, 500.0),
            jpeg_quality: raw
                .cache
                .jpeg_quality
                .clamp(
                    i16::from(MIN_CACHE_JPEG_QUALITY),
                    i16::from(MAX_CACHE_JPEG_QUALITY),
                )
                .try_into()
                .expect("clamped JPEG quality fits in u8"),
            binds,
        }
    }

    pub fn pressed(&self, input: &egui::InputState, action: Action) -> bool {
        self.binds
            .get(&action)
            .is_some_and(|binds| binds.iter().any(|b| b.is_pressed(input)))
    }

    /// First rating action whose bind fired this frame.
    pub fn pressed_rating(&self, input: &egui::InputState) -> Option<u8> {
        (0..=5).find(|&n| self.pressed(input, Action::Rate(n)))
    }

    pub fn binds_of(&self, action: Action) -> &[Bind] {
        self.binds.get(&action).map_or(&[], |v| v.as_slice())
    }

    pub fn add_bind(&mut self, action: Action, bind: Bind) {
        let binds = self.binds.entry(action).or_default();
        if !binds.contains(&bind) {
            binds.push(bind);
        }
    }

    pub fn remove_bind(&mut self, action: Action, bind: Bind) {
        if let Some(binds) = self.binds.get_mut(&action) {
            binds.retain(|b| *b != bind);
        }
    }

    /// Regenerate viewr.toml from the current state. (Hand-edited
    /// comments are replaced — the file is round-tripped by the app.)
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        let mut out = String::from(
            "# viewr configuration. Managed by the Preferences window;\n\
             # hand-edits are read at startup but comments are not kept.\n\n[input]\n",
        );
        out.push_str(&format!(
            "scroll = \"{}\"\n\n[ui]\ntier_indicator = \"{}\"\nshow_loading = {}\nshow_performance = {}\nshow_exposure = {}\nfilmstrip_height = {:.0}\ngrid_cell = {:.0}\n\n[cache]\nram_gb = {:.1}\ndisk_gb = {:.1}\njpeg_quality = {}\n\n[binds]\n",
            match self.scroll {
                ScrollMode::Pan => "pan",
                ScrollMode::Zoom => "zoom",
            },
            self.tier_indicator.as_str(),
            self.show_loading,
            self.show_performance,
            self.show_exposure,
            self.filmstrip_height,
            self.grid_cell,
            self.ram_gb,
            self.disk_gb,
            self.jpeg_quality,
        ));
        for &(name, _, action) in ACTIONS {
            let labels: Vec<String> = self
                .binds_of(action)
                .iter()
                .map(|b| format!("\"{}\"", b.label()))
                .collect();
            out.push_str(&format!("{name} = [{}]\n", labels.join(", ")));
        }
        if let Err(e) = std::fs::write(&path, out) {
            eprintln!("failed to save {}: {e}", path.display());
        }
    }
}

fn finite_clamp(value: f32, fallback: f32, minimum: f32, maximum: f32) -> f32 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        fallback
    }
}

fn config_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("viewr");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("viewr.toml"))
}

const TEMPLATE: &str = r#"# viewr configuration.
# Missing keys fall back to defaults, so override only what you want.
# The in-app Preferences window (Cmd+,) edits and saves this file.

[input]
# "pan":  plain scroll pans the zoomed image; pinch or Ctrl/Cmd+scroll zooms.
# "zoom": plain scroll zooms.
scroll = "pan"

[ui]
# Cache state below each thumbnail: "marks", "border", or "hidden".
# Marks use green double checks for full resolution, amber single checks for
# browse resolution, and a blue dot for compressed cache data.
tier_indicator = "marks"
# Show the current-image message while zoom waits for full resolution.
show_loading = true
# Show load time and RAM/JPEG cache usage in the toolbar.
show_performance = true
# Show ISO, shutter, aperture, and focal length in the toolbar.
show_exposure = true

[cache]
# Total RAM cache budget in GB, including thumbnails.
ram_gb = 4.5
# Disk cache budget in GB (~/Library/Caches/viewr).
disk_gb = 20.0
# Cache JPEG quality from 80 to 100. Higher values retain smoother gradients
# and more detail but use more RAM, disk space, and background CPU time.
jpeg_quality = 97

[binds]
# Each action takes a list of binds: "Key" or "Mod+Key".
# Modifiers: Cmd, Ctrl, Alt, Shift. Keys use egui names
# (A-Z, 0-9, ArrowLeft/Right/Up/Down, Space, Home, End, Enter, Comma...).
# Defaults shown below — uncomment to change.
#next = ["ArrowRight"]
#prev = ["ArrowLeft"]
#first = ["Home"]
#last = ["End"]
#toggle_zoom = ["Space", "Z"]
#grid = ["G"]
#metadata = ["I"]
#fullscreen = ["F"]
#open_folder = ["Cmd+O"]
#preferences = ["Cmd+Comma"]
#rate_0 = ["0"]
#rate_1 = ["1"]
#rate_2 = ["2"]
#rate_3 = ["3"]
#rate_4 = ["4"]
#rate_5 = ["5"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_binds() {
        let b = parse_bind("Cmd+Shift+O").unwrap();
        assert_eq!(b.key, egui::Key::O);
        assert!(b.mods.command && b.mods.shift && !b.mods.ctrl);
        let b = parse_bind("ArrowRight").unwrap();
        assert_eq!(b.key, egui::Key::ArrowRight);
        assert_eq!(b.mods, egui::Modifiers::NONE);
        let b = parse_bind("3").unwrap();
        assert_eq!(b.key, egui::Key::Num3);
        assert!(parse_bind("NotAKey").is_none());
    }

    #[test]
    fn parse_bind_accepts_modifier_aliases_and_rejects_missing_keys() {
        let bind = parse_bind(" Control + Option + Comma ").unwrap();
        assert_eq!(bind.key, egui::Key::Comma);
        assert!(bind.mods.ctrl && bind.mods.alt);
        assert!(!bind.mods.command && !bind.mods.shift);

        let bind = parse_bind("Super+Shift+0").unwrap();
        assert_eq!(bind.key, egui::Key::Num0);
        assert!(bind.mods.command && bind.mods.shift);

        for invalid in ["", "Cmd", "Ctrl+DefinitelyNotAKey"] {
            assert!(parse_bind(invalid).is_none(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn raw_overrides_merge_over_defaults() {
        let raw: RawConfig = toml::from_str(
            r#"
            [input]
            scroll = "zoom"
            [cache]
            ram_gb = 8.0
            jpeg_quality = 91
            [binds]
            next = ["D"]
            "#,
        )
        .unwrap();
        let cfg = Config::from_raw(raw);
        assert_eq!(cfg.scroll, ScrollMode::Zoom);
        assert!((cfg.ram_gb - 8.0).abs() < f32::EPSILON);
        assert_eq!(cfg.jpeg_quality, 91);
        assert_eq!(cfg.tier_indicator, TierIndicator::Marks);
        assert!(cfg.show_loading);
        assert!(cfg.show_performance);
        assert!(cfg.show_exposure);
        assert_eq!(cfg.binds[&Action::Next].len(), 1);
        assert_eq!(cfg.binds[&Action::Next][0].key, egui::Key::D);
        assert_eq!(cfg.binds[&Action::Prev][0].key, egui::Key::ArrowLeft);
    }

    #[test]
    fn values_are_clamped_to_supported_ranges() {
        let too_low: RawConfig = toml::from_str(
            r#"
            [ui]
            filmstrip_height = -10.0
            grid_cell = -10.0
            [cache]
            ram_gb = -10.0
            disk_gb = -10.0
            jpeg_quality = -10
            "#,
        )
        .unwrap();
        let cfg = Config::from_raw(too_low);
        assert_eq!(cfg.filmstrip_height, 70.0);
        assert_eq!(cfg.grid_cell, 120.0);
        assert_eq!(cfg.ram_gb, 1.0);
        assert_eq!(cfg.disk_gb, 1.0);
        assert_eq!(cfg.jpeg_quality, MIN_CACHE_JPEG_QUALITY);

        let too_high: RawConfig = toml::from_str(
            r#"
            [ui]
            filmstrip_height = 1000.0
            grid_cell = 1000.0
            [cache]
            ram_gb = 1000.0
            disk_gb = 1000.0
            jpeg_quality = 1000
            "#,
        )
        .unwrap();
        let cfg = Config::from_raw(too_high);
        assert_eq!(cfg.filmstrip_height, 320.0);
        assert_eq!(cfg.grid_cell, 400.0);
        assert_eq!(cfg.ram_gb, 20.0);
        assert_eq!(cfg.disk_gb, 500.0);
        assert_eq!(cfg.jpeg_quality, MAX_CACHE_JPEG_QUALITY);
    }

    #[test]
    fn non_finite_numeric_values_restore_safe_defaults() {
        let mut raw = RawConfig::default();
        raw.ui.filmstrip_height = f32::NAN;
        raw.ui.grid_cell = f32::INFINITY;
        raw.cache.ram_gb = f32::NEG_INFINITY;
        raw.cache.disk_gb = f32::NAN;

        let cfg = Config::from_raw(raw);

        assert_eq!(cfg.filmstrip_height, RawUi::default().filmstrip_height);
        assert_eq!(cfg.grid_cell, RawUi::default().grid_cell);
        assert_eq!(cfg.ram_gb, RawCache::default().ram_gb);
        assert_eq!(cfg.disk_gb, RawCache::default().disk_gb);
    }

    #[test]
    fn explicit_bind_lists_replace_defaults_and_drop_invalid_entries() {
        let raw: RawConfig = toml::from_str(
            r#"
            [binds]
            next = []
            prev = ["DefinitelyNotAKey", "A"]
            first = ["DefinitelyNotAKey"]
            unknown_action = ["B"]
            "#,
        )
        .unwrap();

        let cfg = Config::from_raw(raw);

        assert!(cfg.binds_of(Action::Next).is_empty());
        assert_eq!(cfg.binds_of(Action::Prev), &[parse_bind("A").unwrap()]);
        assert!(cfg.binds_of(Action::First).is_empty());
        assert_eq!(
            cfg.binds_of(Action::Grid),
            &[parse_bind("G").unwrap()],
            "unmentioned known actions retain their defaults"
        );
    }

    #[test]
    fn add_bind_deduplicates_and_remove_bind_matches_modifiers_exactly() {
        let mut cfg = Config::from_raw(RawConfig::default());
        let plain = parse_bind("D").unwrap();
        let modified = parse_bind("Cmd+D").unwrap();

        cfg.add_bind(Action::Next, plain);
        cfg.add_bind(Action::Next, plain);
        cfg.add_bind(Action::Next, modified);
        assert_eq!(
            cfg.binds_of(Action::Next),
            &[parse_bind("ArrowRight").unwrap(), plain, modified,]
        );

        cfg.remove_bind(Action::Next, plain);
        cfg.remove_bind(Action::Next, plain);
        assert_eq!(
            cfg.binds_of(Action::Next),
            &[parse_bind("ArrowRight").unwrap(), modified]
        );
    }

    #[test]
    fn bind_mutation_handles_an_action_without_an_existing_entry() {
        let mut cfg = Config::from_raw(RawConfig::default());
        let bind = parse_bind("Q").unwrap();
        cfg.binds.remove(&Action::Grid);

        cfg.remove_bind(Action::Grid, bind);
        assert!(cfg.binds_of(Action::Grid).is_empty());

        cfg.add_bind(Action::Grid, bind);
        assert_eq!(cfg.binds_of(Action::Grid), &[bind]);
    }

    #[test]
    fn documented_template_parses_to_the_documented_defaults() {
        let raw: RawConfig = toml::from_str(TEMPLATE).unwrap();
        let cfg = Config::from_raw(raw);

        assert_eq!(cfg.scroll, ScrollMode::Pan);
        assert_eq!(cfg.tier_indicator, TierIndicator::Marks);
        assert!(cfg.show_loading);
        assert!(cfg.show_performance);
        assert!(cfg.show_exposure);
        assert_eq!(cfg.filmstrip_height, 112.0);
        assert_eq!(cfg.grid_cell, 200.0);
        assert_eq!(cfg.ram_gb, 4.5);
        assert_eq!(cfg.disk_gb, 20.0);
        assert_eq!(cfg.jpeg_quality, CACHE_JPEG_QUALITY);
    }

    #[test]
    fn legacy_tier_border_migrates_to_marks_or_hidden() {
        let enabled: RawConfig = toml::from_str(
            r#"
            [ui]
            tier_border = true
            "#,
        )
        .unwrap();
        assert_eq!(
            Config::from_raw(enabled).tier_indicator,
            TierIndicator::Marks
        );

        let disabled: RawConfig = toml::from_str(
            r#"
            [ui]
            tier_border = false
            "#,
        )
        .unwrap();
        assert_eq!(
            Config::from_raw(disabled).tier_indicator,
            TierIndicator::Hidden
        );

        let explicit: RawConfig = toml::from_str(
            r#"
            [ui]
            tier_indicator = "border"
            tier_border = false
            "#,
        )
        .unwrap();
        assert_eq!(
            Config::from_raw(explicit).tier_indicator,
            TierIndicator::Border
        );
    }

    #[test]
    fn bind_labels_round_trip_through_parser() {
        for &(_, _, action) in ACTIONS {
            for spec in default_binds(action) {
                let bind = parse_bind(spec).unwrap();
                let reparsed = parse_bind(&bind.label()).unwrap();
                assert_eq!(bind, reparsed, "spec {spec}");
            }
        }
    }
}
