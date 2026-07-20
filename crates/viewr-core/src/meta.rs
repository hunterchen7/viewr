//! Per-file metadata extracted from the raw container (no pixel decode).

use rawler::decoders::RawMetadata;

use crate::types::Orient;

#[derive(Debug, Clone, Default)]
pub struct FileMeta {
    pub orient: Orient,
    /// In-camera rating, if the body wrote one (lowest precedence source).
    pub rating: Option<u32>,
    pub camera: String,
    pub lens: Option<String>,
    pub iso: Option<u16>,
    /// e.g. "1/1600"
    pub shutter: Option<String>,
    /// e.g. "f/6.3"
    pub aperture: Option<String>,
    pub focal_mm: Option<f32>,
    /// EXIF DateTimeOriginal, as written by the camera.
    pub taken: Option<String>,
}

impl FileMeta {
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
                .or_else(|| exif.lens_model.clone()),
            iso: exif.iso_speed_ratings,
            shutter: exif.exposure_time.map(|r| {
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
            taken: exif
                .date_time_original
                .clone()
                .or_else(|| exif.create_date.clone()),
        }
    }
}
