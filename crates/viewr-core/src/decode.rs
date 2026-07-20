//! rawler wrappers: file → RawImage (CFA mosaic) + RawMetadata.

use std::path::Path;
use std::time::{Duration, Instant};

use rawler::RawImage;
use rawler::decoders::{RawDecodeParams, RawMetadata};
use rawler::rawsource::RawSource;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rawler: {0}")]
    Rawler(#[from] rawler::RawlerError),
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
