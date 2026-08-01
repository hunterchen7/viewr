use chrono::{DateTime, Local, Utc};
use eframe::egui;
use viewr_core::folder::FolderEntry;
use viewr_core::meta::FileMeta;

use crate::config::{ImageInfoConfig, ImageInfoPosition};

const MAX_VALUE_CHARS: usize = 512;
const MAX_ITEM_CHARS: usize = MAX_VALUE_CHARS + 32;
const STRIP_HEIGHT: f32 = 26.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageInfoField {
    FileName,
    Captured,
    Modified,
    Camera,
    Lens,
    Iso,
    Shutter,
    Aperture,
    FocalLength,
    FileSize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageInfoItem {
    field: ImageInfoField,
    text: String,
}

/// Builds the complete, ordered strip view-model without touching egui state.
pub(crate) fn build_items(
    config: &ImageInfoConfig,
    entry: &FolderEntry,
    metadata: Option<&FileMeta>,
) -> Vec<ImageInfoItem> {
    if !config.enabled || !config.fields.any_enabled() {
        return Vec::new();
    }

    let fields = config.fields;
    let mut items = Vec::with_capacity(10);
    if fields.file_name {
        push_item(
            &mut items,
            ImageInfoField::FileName,
            "File",
            &entry.file_name,
        );
    }
    if fields.captured
        && let Some(captured) = metadata.and_then(|metadata| metadata.captured.as_ref())
    {
        push_item(
            &mut items,
            ImageInfoField::Captured,
            "Captured",
            &captured.to_string(),
        );
    }
    if fields.modified
        && let Some(modified) = format_modified_time(entry.mtime_ns)
    {
        push_item(&mut items, ImageInfoField::Modified, "Modified", &modified);
    }
    if fields.camera
        && let Some(metadata) = metadata
    {
        push_item(
            &mut items,
            ImageInfoField::Camera,
            "Camera",
            &metadata.camera,
        );
    }
    if fields.lens
        && let Some(lens) = metadata.and_then(|metadata| metadata.lens.as_deref())
    {
        push_item(&mut items, ImageInfoField::Lens, "Lens", lens);
    }
    if fields.iso
        && let Some(iso) = metadata.and_then(|metadata| metadata.iso)
    {
        push_item(&mut items, ImageInfoField::Iso, "ISO", &iso.to_string());
    }
    if fields.shutter
        && let Some(shutter) = metadata.and_then(|metadata| metadata.shutter.as_deref())
    {
        push_item(&mut items, ImageInfoField::Shutter, "Shutter", shutter);
    }
    if fields.aperture
        && let Some(aperture) = metadata.and_then(|metadata| metadata.aperture.as_deref())
    {
        push_item(&mut items, ImageInfoField::Aperture, "Aperture", aperture);
    }
    if fields.focal_length
        && let Some(focal_mm) = metadata
            .and_then(|metadata| metadata.focal_mm)
            .filter(|value| value.is_finite() && *value >= 0.0)
    {
        let focal = if focal_mm.fract().abs() < f32::EPSILON {
            format!("{focal_mm:.0} mm")
        } else {
            format!("{focal_mm:.1} mm")
        };
        push_item(&mut items, ImageInfoField::FocalLength, "Focal", &focal);
    }
    if fields.file_size {
        push_item(
            &mut items,
            ImageInfoField::FileSize,
            "Size",
            &format_file_size(entry.size),
        );
    }
    items
}

fn push_item(items: &mut Vec<ImageInfoItem>, field: ImageInfoField, label: &str, value: &str) {
    let Some(value) = compact_value(value) else {
        return;
    };
    let text = format!("{label} {value}");
    debug_assert!(text.chars().count() <= MAX_ITEM_CHARS);
    items.push(ImageInfoItem { field, text });
}

fn compact_value(value: &str) -> Option<String> {
    let mut compact = String::new();
    let mut chars = 0;
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !compact.is_empty();
            continue;
        }
        if chars >= MAX_VALUE_CHARS {
            break;
        }
        if pending_space && chars < MAX_VALUE_CHARS {
            compact.push(' ');
            chars += 1;
        }
        pending_space = false;
        if chars >= MAX_VALUE_CHARS {
            break;
        }
        compact.push(character);
        chars += 1;
    }
    (!compact.is_empty()).then_some(compact)
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = KB * 1_000.0;
    const GB: f64 = MB * 1_000.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn format_modified_time(mtime_ns: i64) -> Option<String> {
    if mtime_ns <= 0 {
        return None;
    }
    let seconds = mtime_ns.div_euclid(1_000_000_000);
    let nanoseconds = mtime_ns.rem_euclid(1_000_000_000) as u32;
    let utc = DateTime::<Utc>::from_timestamp(seconds, nanoseconds)?;
    Some(
        utc.with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
    )
}

/// Renders a fixed-height, non-overlay strip and returns its consumed rectangle.
pub(crate) fn show(
    ui: &mut egui::Ui,
    position: ImageInfoPosition,
    items: &[ImageInfoItem],
) -> Option<egui::Rect> {
    if items.is_empty() {
        return None;
    }
    let compact = items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("  ·  ");
    let details = items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let panel = match position {
        ImageInfoPosition::Above => egui::Panel::top("image-information"),
        ImageInfoPosition::Below => egui::Panel::bottom("image-information"),
    };
    Some(
        panel
            .exact_size(STRIP_HEIGHT)
            .show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(compact).weak().size(12.0)).truncate(),
                    )
                    .on_hover_text(details);
                });
            })
            .response
            .rect,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use eframe::egui;
    use viewr_core::folder::FolderEntry;
    use viewr_core::meta::{CaptureTimestamp, FileMeta};

    use super::*;
    use crate::config::{ImageInfoConfig, ImageInfoFields, ImageInfoPosition};

    fn entry() -> FolderEntry {
        FolderEntry {
            path: PathBuf::from("/photos/test.ARW"),
            file_name: "test.ARW".into(),
            size: 24_500_000,
            mtime_ns: 1_704_164_645_000_000_000,
        }
    }

    fn metadata() -> FileMeta {
        FileMeta {
            camera: "Sony A1".into(),
            lens: Some("FE 50mm F1.2 GM".into()),
            iso: Some(800),
            shutter: Some("1/1000".into()),
            aperture: Some("f/2.8".into()),
            focal_mm: Some(50.0),
            captured: CaptureTimestamp::from_exif_parts(
                "2024:01:02 03:04:05",
                Some("25"),
                Some("-07:00"),
            ),
            ..FileMeta::default()
        }
    }

    fn only(field: ImageInfoField) -> ImageInfoConfig {
        let mut fields = ImageInfoFields::none();
        match field {
            ImageInfoField::FileName => fields.file_name = true,
            ImageInfoField::Captured => fields.captured = true,
            ImageInfoField::Modified => fields.modified = true,
            ImageInfoField::Camera => fields.camera = true,
            ImageInfoField::Lens => fields.lens = true,
            ImageInfoField::Iso => fields.iso = true,
            ImageInfoField::Shutter => fields.shutter = true,
            ImageInfoField::Aperture => fields.aperture = true,
            ImageInfoField::FocalLength => fields.focal_length = true,
            ImageInfoField::FileSize => fields.file_size = true,
        }
        ImageInfoConfig {
            enabled: true,
            position: ImageInfoPosition::Above,
            fields,
        }
    }

    #[test]
    fn each_field_is_independently_selectable_in_a_stable_order() {
        let all = build_items(&ImageInfoConfig::default(), &entry(), Some(&metadata()));
        assert_eq!(
            all.iter().map(|item| item.field).collect::<Vec<_>>(),
            [
                ImageInfoField::FileName,
                ImageInfoField::Captured,
                ImageInfoField::Modified,
                ImageInfoField::Camera,
                ImageInfoField::Lens,
                ImageInfoField::Iso,
                ImageInfoField::Shutter,
                ImageInfoField::Aperture,
                ImageInfoField::FocalLength,
                ImageInfoField::FileSize,
            ]
        );

        for field in all.iter().map(|item| item.field) {
            let selected = build_items(&only(field), &entry(), Some(&metadata()));
            assert_eq!(selected.len(), 1, "field {field:?}");
            assert_eq!(selected[0].field, field);
        }
    }

    #[test]
    fn disabled_or_unavailable_fields_produce_no_strip_items() {
        let mut config = ImageInfoConfig {
            fields: ImageInfoFields::none(),
            ..ImageInfoConfig::default()
        };
        assert!(build_items(&config, &entry(), Some(&metadata())).is_empty());

        config.fields.camera = true;
        assert!(build_items(&config, &entry(), None).is_empty());

        config.enabled = false;
        config.fields.file_name = true;
        assert!(build_items(&config, &entry(), Some(&metadata())).is_empty());
    }

    #[test]
    fn entry_fields_remain_available_while_raw_metadata_is_pending() {
        let mut fields = ImageInfoFields::none();
        fields.file_name = true;
        fields.modified = true;
        fields.file_size = true;
        let config = ImageInfoConfig {
            fields,
            ..ImageInfoConfig::default()
        };

        let items = build_items(&config, &entry(), None);

        assert_eq!(
            items.iter().map(|item| item.field).collect::<Vec<_>>(),
            [
                ImageInfoField::FileName,
                ImageInfoField::Modified,
                ImageInfoField::FileSize,
            ]
        );
    }

    #[test]
    fn blank_and_control_heavy_metadata_is_omitted_or_bounded_to_one_line() {
        let mut entry = entry();
        entry.file_name = format!("line one\nline two {}", "é".repeat(MAX_VALUE_CHARS * 2));
        entry.mtime_ns = 0;
        let mut meta = metadata();
        meta.camera = " \n\t ".into();
        meta.lens = Some("\0\u{7} lens\nname ".into());
        meta.captured = None;

        let items = build_items(&ImageInfoConfig::default(), &entry, Some(&meta));

        assert!(
            !items
                .iter()
                .any(|item| item.field == ImageInfoField::Camera)
        );
        assert!(
            !items
                .iter()
                .any(|item| item.field == ImageInfoField::Captured)
        );
        assert!(
            !items
                .iter()
                .any(|item| item.field == ImageInfoField::Modified)
        );
        assert!(
            items
                .iter()
                .all(|item| !item.text.chars().any(char::is_control))
        );
        assert!(
            items
                .iter()
                .all(|item| item.text.chars().count() <= MAX_ITEM_CHARS)
        );
    }

    fn layout(
        position: ImageInfoPosition,
        items: &[ImageInfoItem],
    ) -> (Option<egui::Rect>, egui::Rect, egui::Rect) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(320.0, 240.0),
            )),
            ..Default::default()
        };
        let mut info_rect = None;
        let mut loupe_rect = egui::Rect::NOTHING;
        let mut filmstrip_rect = egui::Rect::NOTHING;
        let _ = ctx.run_ui(input, |ui| {
            filmstrip_rect = egui::Panel::bottom("test-filmstrip")
                .exact_size(70.0)
                .show(ui, |_| {})
                .response
                .rect;
            info_rect = show(ui, position, items);
            loupe_rect = ui.available_rect_before_wrap();
        });
        (info_rect, loupe_rect, filmstrip_rect)
    }

    #[test]
    fn strip_consumes_space_above_or_below_without_overlapping_the_loupe() {
        let items = vec![ImageInfoItem {
            field: ImageInfoField::Lens,
            text: format!("Lens {}", "long name ".repeat(100)),
        }];

        let (above, above_loupe, above_filmstrip) = layout(ImageInfoPosition::Above, &items);
        let above = above.expect("above strip");
        assert!(above.bottom() <= above_loupe.top());
        assert!(above_loupe.bottom() <= above_filmstrip.top());
        assert!(above_loupe.is_finite() && above_loupe.height() > 0.0);

        let (below, below_loupe, below_filmstrip) = layout(ImageInfoPosition::Below, &items);
        let below = below.expect("below strip");
        assert!(below_loupe.bottom() <= below.top());
        assert!(below.bottom() <= below_filmstrip.top());
        assert!(below_loupe.is_finite() && below_loupe.height() > 0.0);
    }

    #[test]
    fn empty_items_allocate_no_panel_or_loupe_space() {
        let (empty_info, empty_loupe, _) = layout(ImageInfoPosition::Above, &[]);
        let (_, baseline_loupe, _) = layout_without_info();

        assert!(empty_info.is_none());
        assert_eq!(empty_loupe, baseline_loupe);
    }

    fn layout_without_info() -> (Option<egui::Rect>, egui::Rect, egui::Rect) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(320.0, 240.0),
            )),
            ..Default::default()
        };
        let mut loupe_rect = egui::Rect::NOTHING;
        let mut filmstrip_rect = egui::Rect::NOTHING;
        let _ = ctx.run_ui(input, |ui| {
            filmstrip_rect = egui::Panel::bottom("test-filmstrip")
                .exact_size(70.0)
                .show(ui, |_| {})
                .response
                .rect;
            loupe_rect = ui.available_rect_before_wrap();
        });
        (None, loupe_rect, filmstrip_rect)
    }
}
