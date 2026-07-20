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
    pub tier_border: bool,
    /// Filmstrip panel height in px (drag the divider to change).
    pub filmstrip_height: f32,
    /// Grid cell width in px.
    pub grid_cell: f32,
    /// Total RAM cache budget in GB (rgba ⅔, jpeg ⅓). Applies on the
    /// next folder open.
    pub ram_gb: f32,
    /// Disk cache budget in GB. Applies on the next folder open.
    pub disk_gb: f32,
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
    tier_border: bool,
    filmstrip_height: f32,
    grid_cell: f32,
}
impl Default for RawUi {
    fn default() -> Self {
        Self {
            tier_border: true,
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
}
impl Default for RawCache {
    fn default() -> Self {
        Self {
            ram_gb: 4.5,
            disk_gb: 20.0,
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
        Self {
            scroll: raw.input.scroll,
            tier_border: raw.ui.tier_border,
            filmstrip_height: raw.ui.filmstrip_height.clamp(70.0, 320.0),
            grid_cell: raw.ui.grid_cell.clamp(120.0, 400.0),
            ram_gb: raw.cache.ram_gb.clamp(1.0, 20.0),
            disk_gb: raw.cache.disk_gb.clamp(1.0, 500.0),
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
            "scroll = \"{}\"\n\n[ui]\ntier_border = {}\nfilmstrip_height = {:.0}\ngrid_cell = {:.0}\n\n[cache]\nram_gb = {:.1}\ndisk_gb = {:.1}\n\n[binds]\n",
            match self.scroll {
                ScrollMode::Pan => "pan",
                ScrollMode::Zoom => "zoom",
            },
            self.tier_border,
            self.filmstrip_height,
            self.grid_cell,
            self.ram_gb,
            self.disk_gb,
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
# Subtle border on the main image showing which cache tier is displayed:
# green = full res, amber = browse (half-res), red = thumbnail stand-in.
tier_border = true

[cache]
# RAM cache budget in GB (decoded pixels 2/3, developed-JPEG ring 1/3).
ram_gb = 4.5
# Disk cache budget in GB (~/Library/Caches/viewr).
disk_gb = 20.0

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
    fn raw_overrides_merge_over_defaults() {
        let raw: RawConfig = toml::from_str(
            r#"
            [input]
            scroll = "zoom"
            [cache]
            ram_gb = 8.0
            [binds]
            next = ["D"]
            "#,
        )
        .unwrap();
        let cfg = Config::from_raw(raw);
        assert_eq!(cfg.scroll, ScrollMode::Zoom);
        assert!((cfg.ram_gb - 8.0).abs() < f32::EPSILON);
        assert!(cfg.tier_border); // untouched default
        assert_eq!(cfg.binds[&Action::Next].len(), 1);
        assert_eq!(cfg.binds[&Action::Next][0].key, egui::Key::D);
        assert_eq!(cfg.binds[&Action::Prev][0].key, egui::Key::ArrowLeft);
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
