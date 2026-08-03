//! Parallel decode of baseline JPEGs whose restart interval is one MCU row.
//!
//! Viewr's cache encoder emits a DRI segment whose interval equals exactly one
//! MCU row of the scan. Every restart boundary then lands on a whole-MCU-row
//! boundary and resets the DC predictors, so each run of MCU rows is
//! independently decodable. This module splits such a scan into per-worker
//! chunks, wraps each chunk in a spec-valid mini JPEG (the original header
//! with a patched SOF height, the chunk's entropy bytes, and an EOI), and
//! decodes the chunks with `zune-jpeg` into disjoint slices of one RGBA
//! output allocation.
//!
//! Anything else — no DRI, a mismatched interval, progressive scans, other
//! component layouts, marker anomalies, truncated streams — returns `None`
//! and the caller keeps the ordinary serial decode. The bytes on disk stay a
//! standard baseline JPEG that any decoder can read.

use std::ops::Range;

use rayon::prelude::*;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::types::PixelBuf;

/// Entropy payloads smaller than this decode quickly enough serially that
/// marker scanning and chunk assembly are not worth scheduling.
const MIN_PARALLEL_SCAN_BYTES: usize = 256 * 1024;

/// Decode chunks scheduled per available worker.
const CHUNKS_PER_WORKER: usize = 6;

struct ScanLayout {
    width: usize,
    height: usize,
    /// Pixel rows covered by one MCU row (8 × the maximum vertical sampling).
    mcu_row_px: usize,
    /// Absolute offset of the big-endian SOF height field.
    sof_height_offset: usize,
    /// Offset of the first entropy-coded byte (just after the SOS header).
    entropy_start: usize,
    /// Entropy byte range of each MCU row, excluding the restart markers
    /// between rows and the trailing EOI.
    rows: Vec<Range<usize>>,
}

fn read_segment_length(bytes: &[u8], pos: usize) -> Option<usize> {
    let hi = *bytes.get(pos)?;
    let lo = *bytes.get(pos + 1)?;
    let len = usize::from(hi) << 8 | usize::from(lo);
    (len >= 2 && pos + len <= bytes.len()).then_some(len)
}

/// Parses the header segments and locates every restart-separated MCU row.
///
/// Returns `None` for any stream this module does not prove splittable: the
/// caller must fall back to a whole-stream decode.
fn parse_layout(bytes: &[u8]) -> Option<ScanLayout> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    let mut sof: Option<(usize, usize, usize, usize, usize)> = None;
    let mut dri: Option<usize> = None;
    let entropy_start = loop {
        if *bytes.get(pos)? != 0xFF {
            return None;
        }
        // Skip optional 0xFF fill bytes before the marker code.
        let mut marker = *bytes.get(pos + 1)?;
        while marker == 0xFF {
            pos += 1;
            marker = *bytes.get(pos + 1)?;
        }
        pos += 2;
        match marker {
            // Standalone markers carry no length field.
            0x01 | 0xD0..=0xD7 => {}
            // SOF0: the only frame type with restart-splittable baseline scans
            // that zune-jpeg decodes the way this module assumes.
            0xC0 => {
                let len = read_segment_length(bytes, pos)?;
                let payload = bytes.get(pos + 2..pos + len)?;
                if payload.len() < 6 || payload[0] != 8 {
                    return None;
                }
                let height = usize::from(payload[1]) << 8 | usize::from(payload[2]);
                let width = usize::from(payload[3]) << 8 | usize::from(payload[4]);
                let components = usize::from(payload[5]);
                if components != 3 || payload.len() < 6 + components * 3 {
                    return None;
                }
                let mut max_h = 0usize;
                let mut max_v = 0usize;
                for component in payload[6..6 + components * 3].chunks_exact(3) {
                    max_h = max_h.max(usize::from(component[1] >> 4));
                    max_v = max_v.max(usize::from(component[1] & 0x0F));
                }
                // Vertical chroma subsampling makes fancy chroma upsampling
                // interpolate across MCU-row boundaries, so a row-chunked
                // decode would differ near chunk seams. Viewr's cache objects
                // are 4:4:4; 4:2:2 keeps one chroma row per luma row and its
                // upsampling interpolates only horizontally, so both split
                // exactly. Anything vertically subsampled falls back.
                if !(1..=4).contains(&max_h) || max_v != 1 {
                    return None;
                }
                if sof
                    .replace((width, height, max_h, max_v, pos + 3))
                    .is_some()
                {
                    return None;
                }
                pos += len;
            }
            // Progressive, extended, lossless, hierarchical, or arithmetic
            // frames: not splittable by this module.
            0xC1 | 0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
            // DRI: the restart interval in MCUs.
            0xDD => {
                let len = read_segment_length(bytes, pos)?;
                if len != 4 {
                    return None;
                }
                dri = Some(usize::from(bytes[pos + 2]) << 8 | usize::from(bytes[pos + 3]));
                pos += len;
            }
            // SOS: entropy data follows its header.
            0xDA => {
                let len = read_segment_length(bytes, pos)?;
                break pos + len;
            }
            // EOI or a second SOI before any scan: malformed for our purposes.
            0xD8 | 0xD9 => return None,
            // Tables, comments, and application segments.
            _ => {
                let len = read_segment_length(bytes, pos)?;
                pos += len;
            }
        }
    };

    let (width, height, max_h, max_v, sof_height_offset) = sof?;
    if width == 0 || height == 0 {
        return None;
    }
    let mcu_row_px = 8 * max_v;
    let mcus_per_row = width.div_ceil(8 * max_h);
    let mcu_rows = height.div_ceil(mcu_row_px);
    // The split relies on every restart boundary being a whole-MCU-row
    // boundary, which holds only when the interval is exactly one row.
    if dri != Some(mcus_per_row) || mcu_rows < 2 {
        return None;
    }
    if bytes.len().saturating_sub(entropy_start) < MIN_PARALLEL_SCAN_BYTES {
        return None;
    }

    let mut rows = Vec::with_capacity(mcu_rows);
    let mut row_start = entropy_start;
    let mut finished = false;
    for found in memchr::memchr_iter(0xFF, &bytes[entropy_start..]) {
        let marker_pos = entropy_start + found;
        if marker_pos < row_start {
            // A 0xFF inside a restart marker pair already consumed below.
            continue;
        }
        match *bytes.get(marker_pos + 1)? {
            // Byte stuffing: a literal 0xFF in the entropy stream.
            0x00 => {}
            // Fill byte; the next iteration examines the marker after it.
            0xFF => {}
            0xD0..=0xD7 => {
                rows.push(row_start..marker_pos);
                row_start = marker_pos + 2;
            }
            0xD9 => {
                rows.push(row_start..marker_pos);
                finished = true;
                break;
            }
            // DNL, a second scan, or any other marker: not splittable.
            _ => return None,
        }
    }
    if !finished || rows.len() != mcu_rows {
        return None;
    }

    Some(ScanLayout {
        width,
        height,
        mcu_row_px,
        sof_height_offset,
        entropy_start,
        rows,
    })
}

/// Groups consecutive MCU rows into `target_chunks` spans balanced by entropy
/// byte count, so denser rows do not serialize the slowest worker.
fn chunk_rows(rows: &[Range<usize>], target_chunks: usize) -> Vec<Range<usize>> {
    let total_bytes: usize = rows.iter().map(Range::len).sum();
    let chunks = target_chunks.clamp(1, rows.len());
    let mut out = Vec::with_capacity(chunks);
    let mut start = 0usize;
    let mut cumulative = 0usize;
    for chunk in 0..chunks {
        // Later chunks must keep at least one row each.
        let max_end = rows.len() - (chunks - chunk - 1);
        let want = total_bytes / chunks * (chunk + 1);
        cumulative += rows[start].len();
        let mut end = start + 1;
        while end < max_end && cumulative < want {
            cumulative += rows[end].len();
            end += 1;
        }
        out.push(start..end);
        start = end;
    }
    debug_assert_eq!(start, rows.len());
    out
}

fn decode_chunk(
    bytes: &[u8],
    layout: &ScanLayout,
    chunk: Range<usize>,
    pixel_height: usize,
    out: &mut [u8],
) -> Result<(), ()> {
    let span = layout.rows[chunk.start].start..layout.rows[chunk.end - 1].end;
    let header = &bytes[..layout.entropy_start];
    let mut mini = Vec::with_capacity(header.len() + span.len() + 2);
    mini.extend_from_slice(header);
    mini[layout.sof_height_offset] = (pixel_height >> 8) as u8;
    mini[layout.sof_height_offset + 1] = (pixel_height & 0xFF) as u8;
    mini.extend_from_slice(&bytes[span]);
    mini.extend_from_slice(&[0xFF, 0xD9]);

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(std::io::Cursor::new(mini.as_slice()), options);
    decoder.decode_into(out).map_err(|_| ())?;
    (decoder.dimensions() == Some((layout.width, pixel_height)))
        .then_some(())
        .ok_or(())
}

/// Advisory request to decode one horizontal band of the frame first.
///
/// Vertical extents are image-space V coordinates (0..1). `align_px` expands
/// the band outward to the consumer's tile grid (which starts at row 0) and
/// `gutter_px` adds the consumer's sampling gutter, so the published rows can
/// back complete tiles rather than partial ones.
pub(crate) struct BandRequest {
    pub uv_y0: f32,
    pub uv_y1: f32,
    pub align_px: u32,
    pub gutter_px: u32,
}

/// Rows decoded ahead of the rest of the frame.
///
/// `rows` is a tightly packed RGBA view of image rows `y0..y0 + h` at full
/// width, borrowed from the final output allocation between decode phases.
/// The bytes are exactly what the finished frame will contain for those rows.
pub(crate) struct BandPixels<'a> {
    pub full_width: u32,
    pub full_height: u32,
    pub y0: u32,
    pub rows: &'a [u8],
}

/// Outcome of [`try_decode_prioritized`].
pub(crate) enum PrioritizedDecode {
    /// The whole frame decoded; bytes are identical to a serial decode.
    Done(PixelBuf),
    /// The caller cancelled at the phase boundary. No output was produced and
    /// the input bytes were never judged corrupt, so the caller must not
    /// treat the source object as damaged.
    Cancelled,
    /// The stream is not provably splittable (or a chunk failed to decode);
    /// the caller must fall back to the ordinary serial decode, exactly as
    /// for [`try_decode`] returning `None`.
    Unsupported,
}

/// Expands a UV band request to whole MCU rows aligned to the consumer's
/// tile grid, returning `None` when the request is degenerate or covers the
/// whole frame — both decode single-phase like [`try_decode`].
fn band_mcu_rows(layout: &ScanLayout, band: &BandRequest) -> Option<Range<usize>> {
    if band.uv_y0.is_nan() || band.uv_y1.is_nan() {
        return None;
    }
    let height = layout.height;
    let uv_y0 = band.uv_y0.clamp(0.0, 1.0);
    let uv_y1 = band.uv_y1.clamp(0.0, 1.0);
    if uv_y1 <= uv_y0 {
        return None;
    }
    let y0 = ((uv_y0 * height as f32).floor() as usize).min(height);
    let y1 = ((uv_y1 * height as f32).ceil() as usize).min(height);
    if y1 <= y0 {
        return None;
    }
    // The consumer's tile grid starts at row 0, so aligning outward keeps
    // every touched tile row fully inside the band.
    let align = band.align_px.max(1) as usize;
    let y0 = y0 / align * align;
    let y1 = y1.div_ceil(align).saturating_mul(align).min(height);
    // The consumer samples one gutter beyond each tile edge.
    let gutter = band.gutter_px as usize;
    let y0 = y0.saturating_sub(gutter);
    let y1 = y1.saturating_add(gutter).min(height);
    // Entropy runs exist per whole MCU row only.
    let r0 = y0 / layout.mcu_row_px;
    let r1 = y1.div_ceil(layout.mcu_row_px).min(layout.rows.len());
    (r0 < r1 && !(r0 == 0 && r1 == layout.rows.len())).then_some(r0..r1)
}

type ChunkTask<'a> = (Range<usize>, usize, &'a mut [u8]);

/// Splits one contiguous output region into per-chunk disjoint slices.
///
/// `chunks` are absolute MCU-row ranges that partition the rows backing
/// `region` in ascending order; the carve consumes the region exactly.
fn carve_span<'a>(
    layout: &ScanLayout,
    chunks: &[Range<usize>],
    region: &'a mut [u8],
    row_bytes: usize,
    tasks: &mut Vec<ChunkTask<'a>>,
) {
    let mut rest = region;
    for chunk in chunks {
        let start_px = chunk.start * layout.mcu_row_px;
        let end_px = (chunk.end * layout.mcu_row_px).min(layout.height);
        debug_assert!(start_px < end_px);
        let pixel_height = end_px - start_px;
        let (head, tail) = rest.split_at_mut(pixel_height * row_bytes);
        tasks.push((chunk.clone(), pixel_height, head));
        rest = tail;
    }
    debug_assert!(rest.is_empty());
}

fn run_tasks(bytes: &[u8], layout: &ScanLayout, tasks: Vec<ChunkTask<'_>>) -> Result<(), ()> {
    tasks
        .into_par_iter()
        .try_for_each(|(chunk, pixel_height, out)| {
            decode_chunk(bytes, layout, chunk, pixel_height, out)
        })
}

/// Attempts a parallel decode of a row-aligned restart-marker baseline JPEG.
///
/// Returns `None` whenever the stream is not provably splittable or any chunk
/// fails to decode; the caller then runs the ordinary serial decode, so this
/// path can never produce output the serial decoder would not.
pub(crate) fn try_decode(bytes: &[u8]) -> Option<PixelBuf> {
    match try_decode_prioritized(bytes, None, &|| false, &mut |_| {}) {
        PrioritizedDecode::Done(buf) => Some(buf),
        PrioritizedDecode::Cancelled | PrioritizedDecode::Unsupported => None,
    }
}

/// Parallel decode with an optional visible-band-first schedule.
///
/// Without a band this is exactly [`try_decode`]. With one, the MCU rows
/// covering the band decode first (phase 1, using every worker), the band is
/// exposed to `on_band` as a borrow of the final output allocation,
/// `cancelled` is consulted once at the phase boundary, and the remaining
/// rows decode second.
///
/// Exactness: chunk grouping and phase ordering cannot change decoded bytes.
/// Every chunk is an independently decodable restart-marker run handled by
/// the same `decode_chunk` into the same disjoint rows of one allocation, and
/// per-row output is independent of how rows are grouped — the codebase
/// already relies on this because the chunk count varies with the worker
/// count. The final buffer is therefore byte-identical to [`try_decode`] and
/// to the serial decoder, and the band rows are byte-identical to the same
/// rows of that final buffer.
pub(crate) fn try_decode_prioritized(
    bytes: &[u8],
    band: Option<BandRequest>,
    cancelled: &dyn Fn() -> bool,
    on_band: &mut dyn FnMut(BandPixels<'_>),
) -> PrioritizedDecode {
    let threads = rayon::current_num_threads();
    if threads < 2 {
        return PrioritizedDecode::Unsupported;
    }
    let Some(layout) = parse_layout(bytes) else {
        return PrioritizedDecode::Unsupported;
    };
    let Some(row_bytes) = layout.width.checked_mul(4) else {
        return PrioritizedDecode::Unsupported;
    };
    let Some(total_len) = row_bytes.checked_mul(layout.height) else {
        return PrioritizedDecode::Unsupported;
    };
    // Several chunks per worker: entropy density varies between MCU rows, so
    // exactly one chunk per worker leaves the join waiting on the densest
    // chunk. Finer chunks let work stealing absorb the imbalance; per-chunk
    // header decode stays cheap relative to the chunk body.
    let total_target = threads * CHUNKS_PER_WORKER;
    let band_rows = band.and_then(|band| band_mcu_rows(&layout, &band));

    let mut rgba = vec![0u8; total_len];
    let width = layout.width as u32;
    let height = layout.height as u32;

    let Some(band_rows) = band_rows else {
        // Single phase: identical to the historical whole-frame split.
        let chunks = chunk_rows(&layout.rows, total_target);
        if chunks.len() < 2 {
            return PrioritizedDecode::Unsupported;
        }
        let mut tasks = Vec::with_capacity(chunks.len());
        carve_span(&layout, &chunks, rgba.as_mut_slice(), row_bytes, &mut tasks);
        if run_tasks(bytes, &layout, tasks).is_err() {
            return PrioritizedDecode::Unsupported;
        }
        return PrioritizedDecode::Done(PixelBuf {
            width,
            height,
            rgba,
        });
    };

    // Chunk targets proportional to each region's entropy bytes, so phase
    // boundaries do not distort the byte balancing; the band always receives
    // at least one chunk per worker so phase 1 keeps every worker busy.
    let total_scan_bytes: usize = layout.rows.iter().map(Range::len).sum();
    let target_for = |region_bytes: usize| {
        if total_scan_bytes == 0 {
            1
        } else {
            (total_target.saturating_mul(region_bytes))
                .div_ceil(total_scan_bytes)
                .max(1)
        }
    };
    let chunk_region = |rows: &[Range<usize>], offset: usize, minimum: usize| {
        if rows.is_empty() {
            return Vec::new();
        }
        let region_bytes = rows.iter().map(Range::len).sum();
        chunk_rows(rows, target_for(region_bytes).max(minimum))
            .into_iter()
            .map(|chunk| chunk.start + offset..chunk.end + offset)
            .collect::<Vec<_>>()
    };
    let band_chunks = chunk_region(&layout.rows[band_rows.clone()], band_rows.start, threads);
    let prefix_chunks = chunk_region(&layout.rows[..band_rows.start], 0, 1);
    let suffix_chunks = chunk_region(&layout.rows[band_rows.end..], band_rows.end, 1);
    if band_chunks.len() + prefix_chunks.len() + suffix_chunks.len() < 2 {
        return PrioritizedDecode::Unsupported;
    }

    let band_start_px = band_rows.start * layout.mcu_row_px;
    let band_end_px = (band_rows.end * layout.mcu_row_px).min(layout.height);
    let band_byte_range = band_start_px * row_bytes..band_end_px * row_bytes;

    // Phase 1: the visible band, on every worker. The per-chunk mutable
    // slices exist only inside this block, so the immutable whole-allocation
    // borrow taken by the callback below cannot alias them.
    {
        let mut tasks = Vec::with_capacity(band_chunks.len());
        carve_span(
            &layout,
            &band_chunks,
            &mut rgba[band_byte_range.clone()],
            row_bytes,
            &mut tasks,
        );
        if run_tasks(bytes, &layout, tasks).is_err() {
            return PrioritizedDecode::Unsupported;
        }
    }
    on_band(BandPixels {
        full_width: width,
        full_height: height,
        y0: band_start_px as u32,
        rows: &rgba[band_byte_range.clone()],
    });
    if cancelled() {
        return PrioritizedDecode::Cancelled;
    }

    // Phase 2: everything outside the band, in one join.
    {
        let (before_band, rest) = rgba.split_at_mut(band_byte_range.start);
        let after_band = &mut rest[band_byte_range.end - band_byte_range.start..];
        let mut tasks = Vec::with_capacity(prefix_chunks.len() + suffix_chunks.len());
        carve_span(&layout, &prefix_chunks, before_band, row_bytes, &mut tasks);
        carve_span(&layout, &suffix_chunks, after_band, row_bytes, &mut tasks);
        if run_tasks(bytes, &layout, tasks).is_err() {
            return PrioritizedDecode::Unsupported;
        }
    }

    PrioritizedDecode::Done(PixelBuf {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BandRequest, PrioritizedDecode, chunk_rows, parse_layout, try_decode,
        try_decode_prioritized,
    };
    use crate::jobs::{decode_jpeg, encode_jpeg};
    use crate::types::PixelBuf;
    use std::cell::Cell;

    /// Deterministic content with gradients, hard edges, and per-pixel noise
    /// so entropy density varies strongly between rows.
    fn textured_photo(width: u32, height: u32) -> PixelBuf {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        let mut state = 0x9E37_79B9u32;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 24) as u8;
                let edge = if (x / 37 + y / 23) % 2 == 0 { 200 } else { 40 };
                rgba.extend_from_slice(&[
                    ((x * 255) / width.max(1)) as u8,
                    ((y * 255) / height.max(1)) as u8 ^ (noise >> 3),
                    edge ^ (noise >> 2),
                    255,
                ]);
            }
        }
        PixelBuf {
            width,
            height,
            rgba,
        }
    }

    fn serial_reference(bytes: &[u8]) -> PixelBuf {
        use zune_jpeg::JpegDecoder;
        use zune_jpeg::zune_core::colorspace::ColorSpace;
        use zune_jpeg::zune_core::options::DecoderOptions;
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
        let mut decoder = JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
        let pixels = decoder.decode().expect("reference decode succeeds");
        let (w, h) = decoder.dimensions().expect("reference has dimensions");
        PixelBuf {
            width: w as u32,
            height: h as u32,
            rgba: pixels,
        }
    }

    #[test]
    fn parallel_decode_matches_serial_for_production_encodes() {
        for (width, height) in [(64, 48), (257, 129), (1023, 769), (16, 512)] {
            let photo = textured_photo(width, height);
            let encoded = encode_jpeg(&photo, 97).expect("encode succeeds");
            let serial = serial_reference(&encoded);
            match try_decode(&encoded) {
                Some(parallel) => {
                    assert_eq!((parallel.width, parallel.height), (width, height));
                    assert_eq!(parallel.rgba, serial.rgba, "{width}x{height} pixels");
                }
                // Small payloads legitimately fall below the parallel
                // threshold; the public decode still uses the serial path.
                None => assert!(
                    encoded.len() < super::MIN_PARALLEL_SCAN_BYTES,
                    "{width}x{height} unexpectedly refused a parallel decode"
                ),
            }
            let public = decode_jpeg(&encoded).expect("public decode succeeds");
            assert_eq!(public.rgba, serial.rgba);
        }
    }

    #[test]
    fn band_first_decode_matches_serial_across_band_positions() {
        let (width, height) = (768u32, 1408u32);
        let photo = textured_photo(width, height);
        let encoded = encode_jpeg(&photo, 97).expect("encode succeeds");
        let layout = parse_layout(&encoded).expect("fixture must qualify for the parallel path");
        let serial = serial_reference(&encoded);
        let row_bytes = width as usize * 4;

        for (name, uv_y0, uv_y1) in [
            ("top", 0.0f32, 0.15f32),
            ("middle", 0.42, 0.58),
            ("bottom", 0.9, 1.0),
        ] {
            let mut band_seen: Option<(u32, Vec<u8>)> = None;
            let outcome = try_decode_prioritized(
                &encoded,
                Some(BandRequest {
                    uv_y0,
                    uv_y1,
                    align_px: 256,
                    gutter_px: 1,
                }),
                &|| false,
                &mut |band| {
                    assert!(
                        band_seen.is_none(),
                        "{name}: the band must publish exactly once"
                    );
                    assert_eq!(band.full_width, width, "{name}: band width");
                    assert_eq!(band.full_height, height, "{name}: band height");
                    band_seen = Some((band.y0, band.rows.to_vec()));
                },
            );
            let PrioritizedDecode::Done(parallel) = outcome else {
                panic!("{name}: prioritized decode must complete");
            };
            assert_eq!((parallel.width, parallel.height), (width, height));
            assert_eq!(parallel.rgba, serial.rgba, "{name}: final pixels");

            let (y0, rows) = band_seen.expect("a mid-frame band must publish");
            let y0 = y0 as usize;
            // Band geometry invariants: whole MCU rows only, tight packing,
            // and full coverage of the requested extent after tile alignment
            // and gutter expansion.
            assert_eq!(y0 % layout.mcu_row_px, 0, "{name}: MCU alignment");
            assert_eq!(rows.len() % row_bytes, 0, "{name}: packed rows");
            let band_h = rows.len() / row_bytes;
            let requested_y0 = (uv_y0 * height as f32).floor() as usize;
            let requested_y1 = ((uv_y1 * height as f32).ceil() as usize).min(height as usize);
            assert!(y0 <= requested_y0, "{name}: band starts above the request");
            assert!(
                y0 + band_h >= requested_y1,
                "{name}: band ends below the request"
            );
            assert!(y0 + band_h <= height as usize, "{name}: band inside frame");
            // The published rows equal the same rows of the serial output.
            assert_eq!(
                rows.as_slice(),
                &serial.rgba[y0 * row_bytes..y0 * row_bytes + rows.len()],
                "{name}: band rows"
            );
        }
    }

    #[test]
    fn full_frame_and_degenerate_bands_decode_single_phase() {
        let photo = textured_photo(768, 900);
        let encoded = encode_jpeg(&photo, 97).expect("encode succeeds");
        assert!(parse_layout(&encoded).is_some());
        let serial = serial_reference(&encoded);

        // Full-frame coverage (0..1 with a 1024 tile grid), an empty band,
        // and an inverted band all collapse to the historical single-phase
        // decode without a publication.
        for (uv_y0, uv_y1) in [(0.0f32, 1.0f32), (0.5, 0.5), (0.7, 0.2)] {
            let mut band_calls = 0usize;
            let outcome = try_decode_prioritized(
                &encoded,
                Some(BandRequest {
                    uv_y0,
                    uv_y1,
                    align_px: 1024,
                    gutter_px: 1,
                }),
                &|| false,
                &mut |_| band_calls += 1,
            );
            let PrioritizedDecode::Done(parallel) = outcome else {
                panic!("({uv_y0}, {uv_y1}) must complete");
            };
            assert_eq!(parallel.rgba, serial.rgba, "({uv_y0}, {uv_y1}) pixels");
            assert_eq!(band_calls, 0, "({uv_y0}, {uv_y1}) must not publish");
        }
    }

    #[test]
    fn cancellation_at_the_phase_boundary_keeps_the_published_band_valid() {
        let (width, height) = (768u32, 1408u32);
        let photo = textured_photo(width, height);
        let encoded = encode_jpeg(&photo, 97).expect("encode succeeds");
        assert!(parse_layout(&encoded).is_some());
        let serial = serial_reference(&encoded);

        let cancel = Cell::new(false);
        let mut band_seen: Option<(u32, Vec<u8>)> = None;
        let outcome = try_decode_prioritized(
            &encoded,
            Some(BandRequest {
                uv_y0: 0.4,
                uv_y1: 0.6,
                align_px: 256,
                gutter_px: 1,
            }),
            &|| cancel.get(),
            &mut |band| {
                band_seen = Some((band.y0, band.rows.to_vec()));
                // Flip after publication, exactly like a replan that lands
                // between the two decode phases.
                cancel.set(true);
            },
        );
        assert!(matches!(outcome, PrioritizedDecode::Cancelled));

        let (y0, rows) = band_seen.expect("the band publishes before the cancellation check");
        let row_bytes = width as usize * 4;
        let start = y0 as usize * row_bytes;
        assert_eq!(
            rows.as_slice(),
            &serial.rgba[start..start + rows.len()],
            "cancelled decode still published exact band rows"
        );
    }

    #[test]
    fn fallback_streams_stay_unsupported_through_the_prioritized_entry_point() {
        let photo = textured_photo(512, 384);
        let band = || {
            Some(BandRequest {
                uv_y0: 0.3,
                uv_y1: 0.7,
                align_px: 64,
                gutter_px: 1,
            })
        };

        let mut markerless = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut markerless, 97);
        encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv444);
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("markerless encode succeeds");
        drop(encoder);

        let mut progressive = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut progressive, 97);
        encoder.set_progressive(true);
        encoder.set_restart_interval(512u16.div_ceil(16));
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("progressive encode succeeds");
        drop(encoder);

        let mut subsampled = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut subsampled, 92);
        encoder.set_restart_interval(512u16.div_ceil(16));
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("4:2:0 encode succeeds");
        drop(encoder);

        for (name, stream) in [
            ("markerless", markerless.as_slice()),
            ("progressive", progressive.as_slice()),
            ("4:2:0", subsampled.as_slice()),
        ] {
            let mut band_calls = 0usize;
            assert!(
                matches!(
                    try_decode_prioritized(stream, band(), &|| false, &mut |_| band_calls += 1),
                    PrioritizedDecode::Unsupported
                ),
                "{name} stream must fall back to the serial decode"
            );
            assert_eq!(band_calls, 0, "{name} stream must not publish a band");
        }

        let encoded = encode_jpeg(&textured_photo(1024, 768), 97).expect("encode succeeds");
        assert!(parse_layout(&encoded).is_some());
        for cut in [0, 16, encoded.len() / 2, encoded.len() - 1] {
            let mut band_calls = 0usize;
            assert!(
                matches!(
                    try_decode_prioritized(
                        &encoded[..cut],
                        band(),
                        &|| false,
                        &mut |_| band_calls += 1
                    ),
                    PrioritizedDecode::Unsupported
                ),
                "truncated at {cut}"
            );
            assert_eq!(band_calls, 0, "truncated at {cut} must not publish");
        }
    }

    #[test]
    fn single_thread_pools_fall_back_to_unsupported_without_a_band() {
        let encoded = encode_jpeg(&textured_photo(1023, 769), 97).expect("encode succeeds");
        assert!(parse_layout(&encoded).is_some());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("one-thread pool builds");
        let outcome = pool.install(|| {
            try_decode_prioritized(
                &encoded,
                Some(BandRequest {
                    uv_y0: 0.4,
                    uv_y1: 0.6,
                    align_px: 256,
                    gutter_px: 1,
                }),
                &|| false,
                &mut |_| panic!("the serial fallback must not publish a band"),
            )
        });
        assert!(matches!(outcome, PrioritizedDecode::Unsupported));
    }

    #[test]
    fn parallel_decode_handles_yuv422_row_intervals() {
        let photo = textured_photo(1022, 1280);
        let mut encoded = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut encoded, 92);
        // 4:2:2 halves chroma horizontally only: an MCU covers 16×8 pixels and
        // chroma upsampling never crosses a row boundary.
        encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv422);
        encoder.set_restart_interval(1022u16.div_ceil(16));
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("4:2:2 encode succeeds");
        drop(encoder);

        let layout = parse_layout(&encoded).expect("4:2:2 row-interval stream parses");
        assert_eq!(layout.mcu_row_px, 8);
        assert_eq!(layout.rows.len(), 1280usize.div_ceil(8));
        let parallel = try_decode(&encoded).expect("4:2:2 stream decodes in parallel");
        let serial = serial_reference(&encoded);
        assert_eq!(parallel.rgba, serial.rgba);
    }

    #[test]
    fn vertically_subsampled_streams_fall_back() {
        let photo = textured_photo(1022, 1280);
        let mut encoded = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut encoded, 92);
        // Default 4:2:0 subsampling: fancy chroma upsampling interpolates
        // across MCU rows, so a chunked decode would differ at chunk seams.
        encoder.set_restart_interval(1022u16.div_ceil(16));
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("4:2:0 encode succeeds");
        drop(encoder);

        assert!(parse_layout(&encoded).is_none(), "4:2:0 stream split");
        assert!(decode_jpeg(&encoded).is_ok());
    }

    #[test]
    fn restart_markers_do_not_change_decoded_pixels() {
        let photo = textured_photo(320, 240);
        let with_markers = encode_jpeg(&photo, 97).expect("marker encode succeeds");
        let mut plain = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut plain, 97);
        encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv444);
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("plain encode succeeds");
        drop(encoder);

        assert_eq!(
            serial_reference(&with_markers).rgba,
            serial_reference(&plain).rgba,
            "restart markers changed decoded pixels"
        );
    }

    #[test]
    fn markerless_progressive_and_undersized_streams_fall_back() {
        let photo = textured_photo(512, 384);

        let mut plain = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut plain, 97);
        encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv444);
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("plain encode succeeds");
        drop(encoder);
        assert!(parse_layout(&plain).is_none(), "markerless stream split");
        assert!(decode_jpeg(&plain).is_ok());

        let mut progressive = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut progressive, 97);
        encoder.set_progressive(true);
        encoder.set_restart_interval(512u16.div_ceil(16));
        encoder
            .encode_rgba(&photo.rgba, photo.width, photo.height)
            .expect("progressive encode succeeds");
        drop(encoder);
        assert!(
            parse_layout(&progressive).is_none(),
            "progressive stream split"
        );
        assert!(decode_jpeg(&progressive).is_ok());

        // A single-MCU-row image can never split.
        let short = textured_photo(2048, 8);
        let encoded = encode_jpeg(&short, 97).expect("encode succeeds");
        assert!(parse_layout(&encoded).is_none(), "single-row stream split");
        assert!(decode_jpeg(&encoded).is_ok());
    }

    #[test]
    fn corrupt_streams_fall_back_without_panicking() {
        let photo = textured_photo(1024, 768);
        let encoded = encode_jpeg(&photo, 97).expect("encode succeeds");
        assert!(parse_layout(&encoded).is_some());

        for cut in [0, 1, 2, 3, 16, encoded.len() / 2, encoded.len() - 1] {
            let truncated = &encoded[..cut];
            assert!(super::try_decode(truncated).is_none(), "truncated at {cut}");
            let _ = decode_jpeg(truncated);
        }

        // Corrupting the DRI interval must refuse the split rather than
        // produce wrong geometry.
        let mut wrong_interval = encoded.clone();
        let dri = wrong_interval
            .windows(2)
            .position(|pair| pair == [0xFF, 0xDD])
            .expect("production encode carries DRI");
        wrong_interval[dri + 4] ^= 0x01;
        assert!(parse_layout(&wrong_interval).is_none());
    }

    #[test]
    #[ignore = "requires the pinned public-domain Sony RAW fixture or VIEWR_TEST_RAW"]
    fn real_sony_raw_cache_objects_decode_identically_in_parallel() {
        let path = std::env::var_os("VIEWR_TEST_RAW")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../testdata/real-raw-corpus/HCA04875.ARW")
            });

        for quality in [
            crate::develop::Quality::Browse,
            crate::develop::Quality::Full,
        ] {
            let raw = crate::decode::load(&path).expect("fixture decodes").raw;
            let (developed, _) =
                crate::develop::develop(raw, quality).expect("fixture develop succeeds");
            let encoded = encode_jpeg(&developed, crate::cache_disk::DEFAULT_CACHE_JPEG_QUALITY)
                .expect("cache-quality encode succeeds");
            let parallel =
                try_decode(&encoded).expect("real cache objects qualify for the parallel path");
            let serial = serial_reference(&encoded);
            assert_eq!(
                (parallel.width, parallel.height),
                (serial.width, serial.height),
                "{quality:?} dimensions"
            );
            assert_eq!(parallel.rgba, serial.rgba, "{quality:?} pixels");

            // Band-first schedule over the same real cache object: the final
            // frame and the early band must both match the serial reference.
            let start = std::time::Instant::now();
            let mut band_seen: Option<(std::time::Duration, u32, Vec<u8>)> = None;
            let outcome = try_decode_prioritized(
                &encoded,
                Some(BandRequest {
                    uv_y0: 0.4,
                    uv_y1: 0.6,
                    align_px: 1024,
                    gutter_px: 1,
                }),
                &|| false,
                &mut |band| {
                    band_seen = Some((start.elapsed(), band.y0, band.rows.to_vec()));
                },
            );
            let full_elapsed = start.elapsed();
            let PrioritizedDecode::Done(prioritized) = outcome else {
                panic!("{quality:?} prioritized decode must complete");
            };
            assert_eq!(prioritized.rgba, serial.rgba, "{quality:?} prioritized");
            let (band_elapsed, y0, rows) =
                band_seen.expect("a mid-frame band publishes for real cache objects");
            let row_bytes = serial.width as usize * 4;
            let band_start = y0 as usize * row_bytes;
            assert_eq!(
                rows.as_slice(),
                &serial.rgba[band_start..band_start + rows.len()],
                "{quality:?} band rows"
            );
            eprintln!(
                "{quality:?}: visible band ({} rows) ready in {band_elapsed:?}; full frame in {full_elapsed:?}",
                rows.len() / row_bytes
            );
        }
    }

    #[test]
    fn chunking_covers_every_row_without_empty_chunks() {
        for row_count in [2usize, 3, 9, 64, 583] {
            let rows: Vec<_> = (0..row_count)
                .map(|row| {
                    let start = row * 1000;
                    // Vary the density so byte balancing is exercised.
                    start..start + 100 + (row % 7) * 400
                })
                .collect();
            for target in [1usize, 2, 3, 8, 10, 16, row_count, row_count + 5] {
                let chunks = chunk_rows(&rows, target);
                assert!(!chunks.is_empty());
                assert!(chunks.len() <= target.max(1).min(row_count));
                assert_eq!(chunks[0].start, 0);
                assert_eq!(chunks.last().unwrap().end, row_count);
                for pair in chunks.windows(2) {
                    assert_eq!(pair[0].end, pair[1].start);
                    assert!(!pair[0].is_empty());
                    assert!(!pair[1].is_empty());
                }
            }
        }
    }
}
