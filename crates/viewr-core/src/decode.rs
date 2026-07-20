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
pub enum DecodeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rawler: {0}")]
    Rawler(#[from] rawler::RawlerError),
    #[error("no embedded preview or thumbnail")]
    NoThumb,
    #[error("{0}")]
    Resize(#[from] resize::ResizeError),
}

pub struct ThumbResult {
    pub thumb: PixelBuf,
    pub meta: FileMeta,
}

/// Light pass: metadata + a display-oriented thumbnail from the embedded
/// preview JPEG. No raw pixel decode. (Embedded previews are allowed for
/// thumbnails only — the main view always renders from raw.)
pub fn thumb_and_meta(path: &Path, max_edge: u32) -> Result<ThumbResult, DecodeError> {
    let source = RawSource::new(path)?;
    let decoder = rawler::get_decoder(&source)?;
    let params = RawDecodeParams::default();
    let md = decoder.raw_metadata(&source, &params)?;
    let meta = FileMeta::from_metadata(&md);

    let dyn_img = match decoder.preview_image(&source, &params)? {
        Some(img) => img,
        None => decoder
            .thumbnail_image(&source, &params)?
            .ok_or(DecodeError::NoThumb)?,
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

pub struct DecodedRaw {
    pub raw: RawImage,
    pub metadata: RawMetadata,
    /// Wall time for file read + container parse + decoder construction.
    pub t_open: Duration,
    /// Wall time for metadata-only extraction.
    pub t_metadata: Duration,
    /// Wall time for the entropy decode of the CFA mosaic.
    pub t_raw_decode: Duration,
}

/// Decode a raw file into its CFA mosaic plus metadata.
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
