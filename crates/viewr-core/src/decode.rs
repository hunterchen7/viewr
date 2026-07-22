//! rawler wrappers: file → RawImage (CFA mosaic) + RawMetadata.

use std::path::Path;
use std::time::{Duration, Instant};

use rawler::RawImage;
use rawler::decoders::{RawDecodeParams, RawMetadata};
use rawler::rawsource::RawSource;

use crate::meta::FileMeta;
use crate::resize;
use crate::types::PixelBuf;

#[derive(Debug, thiserror::Error)]
/// Failure while opening a RAW container or extracting its data.
pub enum DecodeError {
    /// The file could not be read.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// `rawler` could not identify or decode the container.
    #[error("rawler: {0}")]
    Rawler(#[from] rawler::RawlerError),
    /// The container exposes no preview, thumbnail, or full embedded image.
    #[error("no embedded preview or thumbnail")]
    NoThumb,
    /// The extracted preview could not be resized.
    #[error("{0}")]
    Resize(#[from] resize::ResizeError),
}

/// Display-oriented embedded thumbnail and metadata returned by one container
/// pass.
pub struct ThumbResult {
    /// Downscaled, orientation-corrected preview pixels.
    pub thumb: PixelBuf,
    /// Metadata extracted from the same decoder instance.
    pub meta: FileMeta,
}

/// Extract container metadata without decoding an embedded preview or raw
/// pixels. Folder-open background work uses this to discover in-camera
/// ratings while thumbnail pixels remain demand-driven.
///
/// # Errors
///
/// Returns [`DecodeError::Io`] for file access failures and
/// [`DecodeError::Rawler`] for unsupported or malformed RAW containers.
pub fn metadata(path: &Path) -> Result<FileMeta, DecodeError> {
    let source = RawSource::new(path)?;
    let decoder = rawler::get_decoder(&source)?;
    let params = RawDecodeParams::default();
    let md = decoder.raw_metadata(&source, &params)?;
    Ok(FileMeta::from_metadata(&md))
}

/// Light pass: metadata + a display-oriented thumbnail from the embedded
/// preview JPEG. No raw pixel decode. The main view can use this demanded
/// thumbnail as a temporary stand-in; its final Browse and Full images are
/// developed from RAW sensor data.
///
/// `max_edge` bounds the long edge after EXIF rotation. A value of zero still
/// produces at least one pixel along each nonzero source dimension.
///
/// # Errors
///
/// In addition to file and RAW-decoder errors, returns [`DecodeError::NoThumb`]
/// when no embedded image exists and [`DecodeError::Resize`] when the decoded
/// pixel buffer cannot be resized.
pub fn thumb_and_meta(path: &Path, max_edge: u32) -> Result<ThumbResult, DecodeError> {
    let source = RawSource::new(path)?;
    let decoder = rawler::get_decoder(&source)?;
    let params = RawDecodeParams::default();
    let md = decoder.raw_metadata(&source, &params)?;
    let meta = FileMeta::from_metadata(&md);

    // Sony's decoder implements only full_image(); try smallest-first the
    // same way dnglab's own extractors fall back.
    let dyn_img = match decoder.preview_image(&source, &params)? {
        Some(img) => img,
        None => match decoder.thumbnail_image(&source, &params)? {
            Some(img) => img,
            None => decoder
                .full_image(&source, &params)?
                .ok_or(DecodeError::NoThumb)?,
        },
    };
    let rgba = dyn_img.to_rgba8();
    let buf = PixelBuf {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    };
    let small = resize::downscale_to_fit(buf, max_edge)?;
    let thumb = resize::apply_orient(small, meta.orient);
    Ok(ThumbResult { thumb, meta })
}

/// Decoded CFA mosaic, container metadata, and per-stage wall-clock timings.
pub struct DecodedRaw {
    /// Sensor-space CFA mosaic and calibration data consumed by development.
    pub raw: RawImage,
    /// Container metadata extracted before the mosaic decode.
    pub metadata: RawMetadata,
    /// Wall time for file read + container parse + decoder construction.
    pub t_open: Duration,
    /// Wall time for metadata-only extraction.
    pub t_metadata: Duration,
    /// Wall time for the entropy decode of the CFA mosaic.
    pub t_raw_decode: Duration,
}

/// Decode a raw file into its CFA mosaic plus metadata.
///
/// Timing fields measure consecutive stages within this call and are intended
/// for diagnostics rather than stable benchmarking.
///
/// # Errors
///
/// Returns [`DecodeError::Io`] for file access failures or
/// [`DecodeError::Rawler`] when the container cannot be parsed or decoded.
pub fn load(path: &Path) -> Result<DecodedRaw, DecodeError> {
    let t = Instant::now();
    let source = RawSource::new(path)?;
    let decoder = rawler::get_decoder(&source)?;
    let params = RawDecodeParams::default();
    let t_open = t.elapsed();

    let t = Instant::now();
    let metadata = decoder.raw_metadata(&source, &params)?;
    let t_metadata = t.elapsed();

    let t = Instant::now();
    let raw = decoder.raw_image(&source, &params, false)?;
    let t_raw_decode = t.elapsed();

    Ok(DecodedRaw {
        raw,
        metadata,
        t_open,
        t_metadata,
        t_raw_decode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Orient;

    #[test]
    #[ignore = "requires the local ignored portrait Sony RAW fixture"]
    fn portrait_arw_thumbnail_uses_embedded_orientation() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/real-raw-corpus/HCA05417.ARW");

        let result = thumb_and_meta(&path, 360).expect("portrait fixture decodes");
        let metadata = metadata(&path).expect("portrait fixture metadata decodes");

        assert_eq!(result.meta.orient, Orient::R270);
        assert_eq!(metadata.orient, result.meta.orient);
        assert_eq!(metadata.rating, result.meta.rating);
        assert_eq!(metadata.camera, result.meta.camera);
        assert_eq!(result.thumb.height, 360);
        assert!(result.thumb.height > result.thumb.width);
        assert_eq!(
            result.thumb.rgba.len(),
            result.thumb.width as usize * result.thumb.height as usize * 4
        );
    }
}
