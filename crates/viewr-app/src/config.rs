//! User configuration: input behavior and keybinds.
//!
//! Lives at `~/Library/Application Support/viewr/viewr.toml`. A
//! documented template is written on first run; absent file or absent
//! keys fall back to defaults, so partial configs are fine.

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

#[derive(Debug, Clone, Copy)]
pub struct Bind {
    key: egui::Key,
    mods: egui::Modifiers,
}

impl Bind {
    /// cmd/ctrl/alt must match exactly; shift is ignored unless the bind
    /// requires it (so Shift+arrow can mean "step 10" on a plain bind).
    fn pressed(&self, input: &egui::InputState) -> bool {
        let m = input.modifiers;
        input.key_pressed(self.key)
            && m.command == self.mods.command
            && m.ctrl == self.mods.ctrl
            && m.alt == self.mods.alt
            && (!self.mods.shift || m.shift)
    }
}

pub struct Config {
    pub scroll: ScrollMode,
    binds: HashMap<Action, Vec<Bind>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    input: RawInput,
    binds: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawInput {
    scroll: ScrollMode,
}

const ACTIONS: &[(&str, Action)] = &[
    ("next", Action::Next),
    ("prev", Action::Prev),
    ("first", Action::First),
    ("last", Action::Last),
    ("toggle_zoom", Action::ToggleZoom),
    ("grid", Action::Grid),
    ("metadata", Action::Metadata),
    ("fullscreen", Action::Fullscreen),
    ("open_folder", Action::OpenFolder),
    ("rate_0", Action::Rate(0)),
    ("rate_1", Action::Rate(1)),
    ("rate_2", Action::Rate(2)),
    ("rate_3", Action::Rate(3)),
    ("rate_4", Action::Rate(4)),
    ("rate_5", Action::Rate(5)),
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
        for &(name, action) in ACTIONS {
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
            binds,
        }
    }

    pub fn pressed(&self, input: &egui::InputState, action: Action) -> bool {
        self.binds
            .get(&action)
            .is_some_and(|binds| binds.iter().any(|b| b.pressed(input)))
    }

    /// First rating action whose bind fired this frame.
    pub fn pressed_rating(&self, input: &egui::InputState) -> Option<u8> {
        (0..=5).find(|&n| self.pressed(input, Action::Rate(n)))
    }
}

fn config_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("viewr");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("viewr.toml"))
}

const TEMPLATE: &str = r#"# viewr configuration.
# Missing keys fall back to defaults, so override only what you want.

[input]
# "pan":  plain scroll pans the zoomed image; pinch or Ctrl/Cmd+scroll zooms.
# "zoom": plain scroll zooms.
scroll = "pan"

[binds]
# Each action takes a list of binds: "Key" or "Mod+Key".
# Modifiers: Cmd, Ctrl, Alt, Shift. Keys use egui names
# (A-Z, 0-9, ArrowLeft/Right/Up/Down, Space, Home, End, Enter, ...).
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
            [binds]
            next = ["D"]
            "#,
        )
        .unwrap();
        let cfg = Config::from_raw(raw);
        assert_eq!(cfg.scroll, ScrollMode::Zoom);
        assert_eq!(cfg.binds[&Action::Next].len(), 1);
        assert_eq!(cfg.binds[&Action::Next][0].key, egui::Key::D);
        // Untouched action keeps its default.
        assert_eq!(cfg.binds[&Action::Prev][0].key, egui::Key::ArrowLeft);
    }
}
