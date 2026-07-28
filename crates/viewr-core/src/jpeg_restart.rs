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

/// Attempts a parallel decode of a row-aligned restart-marker baseline JPEG.
///
/// Returns `None` whenever the stream is not provably splittable or any chunk
/// fails to decode; the caller then runs the ordinary serial decode, so this
/// path can never produce output the serial decoder would not.
pub(crate) fn try_decode(bytes: &[u8]) -> Option<PixelBuf> {
    let threads = rayon::current_num_threads();
    if threads < 2 {
        return None;
    }
    let layout = parse_layout(bytes)?;
    let row_bytes = layout.width.checked_mul(4)?;
    let total_len = row_bytes.checked_mul(layout.height)?;

    let chunks = chunk_rows(&layout.rows, threads);
    if chunks.len() < 2 {
        return None;
    }

    let mut rgba = vec![0u8; total_len];
    let mut tasks = Vec::with_capacity(chunks.len());
    let mut rest = rgba.as_mut_slice();
    for chunk in chunks {
        let start_px = chunk.start * layout.mcu_row_px;
        let end_px = (chunk.end * layout.mcu_row_px).min(layout.height);
        debug_assert!(start_px < end_px);
        let pixel_height = end_px - start_px;
        let (head, tail) = rest.split_at_mut(pixel_height * row_bytes);
        tasks.push((chunk, pixel_height, head));
        rest = tail;
    }
    debug_assert!(rest.is_empty());

    tasks
        .into_par_iter()
        .try_for_each(|(chunk, pixel_height, out)| {
            decode_chunk(bytes, &layout, chunk, pixel_height, out)
        })
        .ok()?;

    Some(PixelBuf {
        width: layout.width as u32,
        height: layout.height as u32,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::{chunk_rows, parse_layout, try_decode};
    use crate::jobs::{decode_jpeg, encode_jpeg};
    use crate::types::PixelBuf;

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
