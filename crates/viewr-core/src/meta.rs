//! Per-file metadata extracted from the raw container (no pixel decode).

use std::fmt;

use chrono::{FixedOffset, NaiveDateTime};
use rawler::decoders::RawMetadata;

use crate::types::Orient;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Camera-recorded local capture time with an optional recorded UTC offset.
///
/// A missing offset stays unknown. Viewr never assigns the computer's timezone
/// to a timestamp that the camera recorded without one.
pub struct CaptureTimestamp {
    local: NaiveDateTime,
    subsecond: Option<String>,
    offset: Option<FixedOffset>,
}

impl CaptureTimestamp {
    /// Parses one matching EXIF date/subsecond/offset triplet.
    ///
    /// The date must use the EXIF `YYYY:MM:DD HH:MM:SS` representation.
    /// Invalid optional modifiers are omitted without invalidating the date.
    pub fn from_exif_parts(
        local: &str,
        subsecond: Option<&str>,
        offset: Option<&str>,
    ) -> Option<Self> {
        let local =
            NaiveDateTime::parse_from_str(trim_exif_text(local), "%Y:%m:%d %H:%M:%S").ok()?;
        Some(Self {
            local,
            subsecond: subsecond.and_then(normalize_subsecond),
            offset: offset.and_then(parse_exif_offset),
        })
    }

    fn with_legacy_offset(mut self, hours: Option<i16>) -> Self {
        if self.offset.is_none() {
            self.offset = hours
                .and_then(|hours| FixedOffset::east_opt(i32::from(hours).checked_mul(60 * 60)?));
        }
        self
    }
}

impl fmt::Display for CaptureTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.local.format("%Y-%m-%d %H:%M:%S"))?;
        if let Some(subsecond) = &self.subsecond {
            write!(formatter, ".{subsecond}")?;
        }
        if let Some(offset) = self.offset {
            write!(formatter, " {offset}")?;
        }
        Ok(())
    }
}

fn trim_exif_text(value: &str) -> &str {
    value.trim_matches(|character: char| character.is_whitespace() || character == '\0')
}

fn normalize_subsecond(value: &str) -> Option<String> {
    let value = trim_exif_text(value);
    (!value.is_empty() && value.len() <= 9 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.to_owned())
}

fn parse_exif_offset(value: &str) -> Option<FixedOffset> {
    let value = trim_exif_text(value);
    let bytes = value.as_bytes();
    if bytes.len() != 6 || bytes[3] != b':' || !matches!(bytes[0], b'+' | b'-') {
        return None;
    }
    let digit = |byte: u8| byte.is_ascii_digit().then_some(i32::from(byte - b'0'));
    let hours = digit(bytes[1])? * 10 + digit(bytes[2])?;
    let minutes = digit(bytes[4])? * 10 + digit(bytes[5])?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let seconds = hours * 60 * 60 + minutes * 60;
    match bytes[0] {
        b'+' => FixedOffset::east_opt(seconds),
        b'-' => FixedOffset::west_opt(seconds),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
/// Compact UI-facing metadata derived from `rawler` container metadata.
pub struct FileMeta {
    /// Display rotation derived from EXIF orientation.
    pub orient: Orient,
    /// In-camera rating, if the body wrote one (lowest precedence source).
    pub rating: Option<u32>,
    /// Trimmed camera make and model.
    pub camera: String,
    /// Lens name from decoded lens metadata, falling back to EXIF LensModel.
    pub lens: Option<String>,
    /// ISO speed rating.
    pub iso: Option<u32>,
    /// e.g. "1/1600"
    pub shutter: Option<String>,
    /// e.g. "f/6.3"
    pub aperture: Option<String>,
    /// Focal length in millimetres.
    pub focal_mm: Option<f32>,
    /// Validated EXIF capture time, preserving an unknown timezone as unknown.
    pub captured: Option<CaptureTimestamp>,
}

impl FileMeta {
    /// Extracts and formats the subset of RAW metadata used by the viewer.
    ///
    /// Invalid zero-denominator exposure, aperture, and focal-length rationals
    /// are omitted. No file or pixel decoding is performed by this conversion.
    pub fn from_metadata(md: &RawMetadata) -> Self {
        let exif = &md.exif;
        Self {
            orient: Orient::from_exif(exif.orientation),
            rating: md.rating,
            camera: format!("{} {}", md.make, md.model).trim().to_string(),
            lens: md
                .lens
                .as_ref()
                .map(|l| l.lens_name.clone())
                .or_else(|| exif.lens_model.clone())
                .and_then(|lens| {
                    let lens = lens.trim().to_owned();
                    (!lens.is_empty()).then_some(lens)
                }),
            iso: exif
                .iso_speed_ratings
                .map(u32::from)
                .or(exif.iso_speed)
                .or(exif.recommended_exposure_index),
            shutter: exif.exposure_time.filter(|r| r.d != 0).map(|r| {
                if r.n == 1 {
                    format!("1/{}", r.d)
                } else if r.d == 1 {
                    format!("{}s", r.n)
                } else {
                    format!("{:.1}s", r.n as f64 / r.d as f64)
                }
            }),
            aperture: exif
                .fnumber
                .filter(|r| r.d != 0)
                .map(|r| format!("f/{:.1}", r.n as f64 / r.d as f64)),
            focal_mm: exif
                .focal_length
                .filter(|r| r.d != 0)
                .map(|r| r.n as f32 / r.d as f32),
            captured: exif
                .date_time_original
                .as_deref()
                .and_then(|date| {
                    CaptureTimestamp::from_exif_parts(
                        date,
                        exif.sub_sec_time_original.as_deref(),
                        exif.offset_time_original.as_deref(),
                    )
                })
                .or_else(|| {
                    exif.create_date.as_deref().and_then(|date| {
                        CaptureTimestamp::from_exif_parts(
                            date,
                            exif.sub_sec_time_digitized.as_deref(),
                            exif.offset_time_digitized.as_deref(),
                        )
                    })
                })
                .map(|timestamp| {
                    timestamp.with_legacy_offset(
                        exif.timezone_offset
                            .as_deref()
                            .and_then(|offsets| offsets.first())
                            .copied(),
                    )
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileMeta;
    use rawler::decoders::RawMetadata;
    use rawler::formats::tiff::Rational;

    #[test]
    fn malformed_zero_denominator_rationals_are_omitted() {
        let mut metadata = RawMetadata::default();
        metadata.exif.exposure_time = Some(Rational { n: 1, d: 0 });
        metadata.exif.fnumber = Some(Rational { n: 28, d: 0 });
        metadata.exif.focal_length = Some(Rational { n: 50, d: 0 });

        let file = FileMeta::from_metadata(&metadata);

        assert_eq!(file.shutter, None);
        assert_eq!(file.aperture, None);
        assert_eq!(file.focal_mm, None);
    }

    #[test]
    fn shutter_rationals_use_the_expected_display_forms() {
        for (rational, expected) in [
            (Rational { n: 1, d: 1_600 }, "1/1600"),
            (Rational { n: 2, d: 1 }, "2s"),
            (Rational { n: 3, d: 2 }, "1.5s"),
        ] {
            let mut metadata = RawMetadata::default();
            metadata.exif.exposure_time = Some(rational);

            assert_eq!(
                FileMeta::from_metadata(&metadata).shutter.as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn capture_time_uses_the_original_matching_subseconds_and_offset() {
        let mut metadata = RawMetadata::default();
        metadata.exif.date_time_original = Some("2024:02:29 23:59:58".into());
        metadata.exif.sub_sec_time_original = Some("125".into());
        metadata.exif.offset_time_original = Some("-07:00".into());
        metadata.exif.create_date = Some("2020:01:02 03:04:05".into());
        metadata.exif.sub_sec_time_digitized = Some("999".into());
        metadata.exif.offset_time_digitized = Some("+02:00".into());

        let file = FileMeta::from_metadata(&metadata);

        assert_eq!(
            file.captured.as_ref().map(ToString::to_string).as_deref(),
            Some("2024-02-29 23:59:58.125 -07:00")
        );
    }

    #[test]
    fn malformed_original_capture_time_falls_back_without_mixing_triplets() {
        let mut metadata = RawMetadata::default();
        metadata.exif.date_time_original = Some("2024:02:30 12:00:00".into());
        metadata.exif.sub_sec_time_original = Some("999".into());
        metadata.exif.offset_time_original = Some("-07:00".into());
        metadata.exif.create_date = Some("2023:12:31 01:02:03".into());
        metadata.exif.sub_sec_time_digitized = Some("42".into());
        metadata.exif.offset_time_digitized = Some("+05:30".into());

        let file = FileMeta::from_metadata(&metadata);

        assert_eq!(
            file.captured.as_ref().map(ToString::to_string).as_deref(),
            Some("2023-12-31 01:02:03.42 +05:30")
        );
    }

    #[test]
    fn capture_time_does_not_infer_a_timezone_and_ignores_invalid_modifiers() {
        let mut metadata = RawMetadata::default();
        metadata.exif.date_time_original = Some("2024:01:02 03:04:05".into());
        metadata.exif.sub_sec_time_original = Some("12not-digits".into());
        metadata.exif.offset_time_original = Some("+99:00".into());

        let file = FileMeta::from_metadata(&metadata);

        assert_eq!(
            file.captured.as_ref().map(ToString::to_string).as_deref(),
            Some("2024-01-02 03:04:05")
        );
    }

    #[test]
    fn iso_uses_modern_fallbacks_without_truncation() {
        let mut metadata = RawMetadata::default();
        metadata.exif.iso_speed = Some(204_800);
        assert_eq!(FileMeta::from_metadata(&metadata).iso, Some(204_800));

        metadata.exif.iso_speed = None;
        metadata.exif.recommended_exposure_index = Some(102_400);
        assert_eq!(FileMeta::from_metadata(&metadata).iso, Some(102_400));

        metadata.exif.iso_speed_ratings = Some(6_400);
        assert_eq!(FileMeta::from_metadata(&metadata).iso, Some(6_400));
    }
}
