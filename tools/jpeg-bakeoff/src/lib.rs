use std::fmt;
use std::io::Cursor;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

pub const QUALITIES: [u8; 4] = [80, 90, 97, 100];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    JpegEncoder,
    JpegRusturbo { threads: u32 },
    JpegRusturboRows { threads: u32 },
    LibjpegTurboRs,
    LibjpegTurboC,
}

impl Codec {
    pub const ALL: [Self; 13] = [
        Self::JpegEncoder,
        Self::JpegRusturbo { threads: 1 },
        Self::JpegRusturbo { threads: 2 },
        Self::JpegRusturbo { threads: 4 },
        Self::JpegRusturbo { threads: 8 },
        Self::JpegRusturbo { threads: 0 },
        Self::JpegRusturboRows { threads: 1 },
        Self::JpegRusturboRows { threads: 2 },
        Self::JpegRusturboRows { threads: 4 },
        Self::JpegRusturboRows { threads: 8 },
        Self::JpegRusturboRows { threads: 0 },
        Self::LibjpegTurboRs,
        Self::LibjpegTurboC,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::JpegEncoder => "jpeg-encoder",
            Self::JpegRusturbo { threads: 1 } => "jpeg-rusturbo-t1",
            Self::JpegRusturbo { threads: 2 } => "jpeg-rusturbo-t2",
            Self::JpegRusturbo { threads: 4 } => "jpeg-rusturbo-t4",
            Self::JpegRusturbo { threads: 8 } => "jpeg-rusturbo-t8",
            Self::JpegRusturbo { threads: 0 } => "jpeg-rusturbo-auto",
            Self::JpegRusturbo { .. } => "jpeg-rusturbo-custom",
            Self::JpegRusturboRows { threads: 1 } => "jpeg-rusturbo-rows-t1",
            Self::JpegRusturboRows { threads: 2 } => "jpeg-rusturbo-rows-t2",
            Self::JpegRusturboRows { threads: 4 } => "jpeg-rusturbo-rows-t4",
            Self::JpegRusturboRows { threads: 8 } => "jpeg-rusturbo-rows-t8",
            Self::JpegRusturboRows { threads: 0 } => "jpeg-rusturbo-rows-auto",
            Self::JpegRusturboRows { .. } => "jpeg-rusturbo-rows-custom",
            Self::LibjpegTurboRs => "libjpeg-turbo-rs",
            Self::LibjpegTurboC => "libjpeg-turbo-c",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug)]
pub struct Fixture {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Fixture {
    pub fn megapixels(&self) -> f64 {
        f64::from(self.width) * f64::from(self.height) / 1_000_000.0
    }
}

#[derive(Clone, Debug)]
pub struct ProbeResult {
    pub codec: Codec,
    pub fixture: &'static str,
    pub width: u32,
    pub height: u32,
    pub quality: u8,
    pub encoded_bytes: usize,
    pub median_encode: Duration,
    pub megapixels_per_second: f64,
    pub psnr_rgb: f64,
    pub max_abs_rgb: u8,
    pub delta_mae: f64,
    pub sampling: Sampling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sampling {
    S444,
    Other,
}

impl fmt::Display for Sampling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::S444 => f.write_str("4:4:4"),
            Self::Other => f.write_str("other"),
        }
    }
}

pub fn fixtures() -> Vec<Fixture> {
    vec![
        synthetic_photo("photo_8mp", 3_504, 2_336),
        dark_gradient("dark_gradient", 2_048, 1_365),
        chroma_edges("chroma_edges", 2_048, 1_365),
        low_entropy("low_entropy", 2_048, 1_365),
    ]
}

pub fn full_resolution_fixture() -> Fixture {
    synthetic_photo("photo_33mp", 7_008, 4_672)
}

pub fn synthetic_photo(name: &'static str, width: u32, height: u32) -> Fixture {
    let mut rgba = allocate_rgba(width, height);
    for y in 0..height {
        for x in 0..width {
            let hash =
                x.wrapping_mul(0x9e37_79b9).rotate_left(y & 15) ^ y.wrapping_mul(0x85eb_ca6b);
            let texture = ((hash ^ (hash >> 13)) & 0x1f) as u8;
            let gradient = ((x * 173 / width.max(1)) + (y * 61 / height.max(1))) as u8;
            let checker = if ((x / 96) + (y / 96)) & 1 == 0 {
                18
            } else {
                0
            };
            set_pixel(
                &mut rgba,
                width,
                x,
                y,
                [
                    gradient.saturating_add(texture / 2),
                    gradient
                        .wrapping_add((x / 19) as u8)
                        .saturating_add(checker),
                    gradient
                        .wrapping_add((y / 17) as u8)
                        .saturating_sub(texture / 3),
                    255,
                ],
            );
        }
    }
    Fixture {
        name,
        width,
        height,
        rgba,
    }
}

fn dark_gradient(name: &'static str, width: u32, height: u32) -> Fixture {
    let mut rgba = allocate_rgba(width, height);
    let denominator = u64::from(width.saturating_sub(1)) + u64::from(height.saturating_sub(1));
    for y in 0..height {
        for x in 0..width {
            // Exercise the dark tones where quantization banding is easiest to see.
            let ramp = (u64::from(x + y) * 62 / denominator.max(1)) as u8;
            let wave = (((x / 37) ^ (y / 29)) & 3) as u8;
            set_pixel(
                &mut rgba,
                width,
                x,
                y,
                [
                    4_u8.saturating_add(ramp).saturating_add(wave),
                    3_u8.saturating_add(ramp),
                    8_u8.saturating_add(ramp).saturating_sub(wave),
                    255,
                ],
            );
        }
    }
    Fixture {
        name,
        width,
        height,
        rgba,
    }
}

fn chroma_edges(name: &'static str, width: u32, height: u32) -> Fixture {
    let mut rgba = allocate_rgba(width, height);
    let palette = [
        [240, 20, 40, 255],
        [20, 220, 50, 255],
        [25, 45, 235, 255],
        [230, 220, 25, 255],
    ];
    for y in 0..height {
        for x in 0..width {
            let tile = ((x / 3) + (y / 5)) as usize % palette.len();
            set_pixel(&mut rgba, width, x, y, palette[tile]);
        }
    }
    Fixture {
        name,
        width,
        height,
        rgba,
    }
}

fn low_entropy(name: &'static str, width: u32, height: u32) -> Fixture {
    let mut rgba = allocate_rgba(width, height);
    for y in 0..height {
        for x in 0..width {
            let value = if x > width / 2 { 151 } else { 147 };
            let stripe = if y % 257 == 0 { 3 } else { 0 };
            set_pixel(
                &mut rgba,
                width,
                x,
                y,
                [value + stripe, value, value.saturating_sub(stripe), 255],
            );
        }
    }
    Fixture {
        name,
        width,
        height,
        rgba,
    }
}

fn allocate_rgba(width: u32, height: u32) -> Vec<u8> {
    vec![0; width as usize * height as usize * 4]
}

fn set_pixel(rgba: &mut [u8], width: u32, x: u32, y: u32, pixel: [u8; 4]) {
    let offset = (y as usize * width as usize + x as usize) * 4;
    rgba[offset..offset + 4].copy_from_slice(&pixel);
}

pub fn encode(codec: Codec, fixture: &Fixture, quality: u8) -> Result<Vec<u8>> {
    validate_input(fixture, quality)?;
    match codec {
        Codec::JpegEncoder => {
            let width = u16::try_from(fixture.width).context("width exceeds JPEG API limit")?;
            let height = u16::try_from(fixture.height).context("height exceeds JPEG API limit")?;
            let mut output = Vec::new();
            let mut encoder = jpeg_encoder::Encoder::new(&mut output, quality);
            encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
            encoder
                .encode(&fixture.rgba, width, height, jpeg_encoder::ColorType::Rgba)
                .context("jpeg-encoder failed")?;
            Ok(output)
        }
        Codec::JpegRusturbo { threads } => encode_rusturbo(fixture, quality, threads, false),
        Codec::JpegRusturboRows { threads } => encode_rusturbo(fixture, quality, threads, true),
        Codec::LibjpegTurboRs => libjpeg_turbo_rs::compress(
            &fixture.rgba,
            fixture.width as usize,
            fixture.height as usize,
            libjpeg_turbo_rs::PixelFormat::Rgba,
            quality,
            libjpeg_turbo_rs::Subsampling::S444,
        )
        .context("libjpeg-turbo-rs failed"),
        Codec::LibjpegTurboC => {
            let mut encoder = TurbojpegEncoder::new(quality)?;
            encoder.encode(fixture)
        }
    }
}

fn encode_rusturbo(
    fixture: &Fixture,
    quality: u8,
    threads: u32,
    restart_rows: bool,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut output, quality);
    encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv444);
    encoder.set_threads(threads);
    if restart_rows {
        encoder.set_restart_interval(fixture.width.div_ceil(8) as u16);
    }
    encoder
        .encode_rgba(&fixture.rgba, fixture.width, fixture.height)
        .context("jpeg-rusturbo failed")?;
    drop(encoder);
    Ok(output)
}

/// Reusable dedicated Rayon pool that mirrors Viewr's background JPEG lane.
pub struct DedicatedRusturboEncoder {
    pool: rayon::ThreadPool,
    quality: u8,
}

impl DedicatedRusturboEncoder {
    pub fn new(workers: usize, quality: u8) -> Result<Self> {
        if workers == 0 {
            bail!("dedicated JPEG pool must have at least one worker");
        }
        if !(1..=100).contains(&quality) {
            bail!("quality must be in 1..=100");
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("jpeg-bakeoff-dedicated-{index}"))
            .build()
            .context("dedicated JPEG pool setup failed")?;
        Ok(Self { pool, quality })
    }

    pub fn workers(&self) -> usize {
        self.pool.current_num_threads()
    }

    pub fn encode(&self, fixture: &Fixture) -> Result<Vec<u8>> {
        validate_input(fixture, self.quality)?;
        self.pool
            .install(|| encode_rusturbo(fixture, self.quality, 0, true))
    }
}

pub struct TurbojpegEncoder {
    compressor: turbojpeg::Compressor,
    quality: u8,
}

impl TurbojpegEncoder {
    pub fn new(quality: u8) -> Result<Self> {
        if !(1..=100).contains(&quality) {
            bail!("quality must be in 1..=100");
        }
        let mut compressor =
            turbojpeg::Compressor::new().context("libjpeg-turbo compressor setup failed")?;
        compressor
            .set_quality(i32::from(quality))
            .context("libjpeg-turbo quality setup failed")?;
        compressor
            .set_subsamp(turbojpeg::Subsamp::None)
            .context("libjpeg-turbo subsampling setup failed")?;
        Ok(Self {
            compressor,
            quality,
        })
    }

    pub fn encode(&mut self, fixture: &Fixture) -> Result<Vec<u8>> {
        validate_input(fixture, self.quality)?;
        self.compressor
            .compress_to_vec(turbojpeg::Image {
                pixels: fixture.rgba.as_slice(),
                width: fixture.width as usize,
                pitch: fixture.width as usize * 4,
                height: fixture.height as usize,
                format: turbojpeg::PixelFormat::RGBA,
            })
            .context("libjpeg-turbo failed")
    }
}

fn validate_input(fixture: &Fixture, quality: u8) -> Result<()> {
    if !(1..=100).contains(&quality) {
        bail!("quality must be in 1..=100");
    }
    if fixture.width == 0 || fixture.height == 0 {
        bail!("zero-sized fixture");
    }
    let expected = usize::try_from(fixture.width)
        .ok()
        .and_then(|width| {
            usize::try_from(fixture.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("fixture dimensions overflow"))?;
    if fixture.rgba.len() != expected {
        bail!(
            "RGBA length mismatch: got {}, expected {expected}",
            fixture.rgba.len()
        );
    }
    Ok(())
}

pub fn probe(codec: Codec, fixture: &Fixture, quality: u8, repeats: usize) -> Result<ProbeResult> {
    if !(1..=100).contains(&quality) {
        bail!("quality must be in 1..=100");
    }
    let repeats = repeats.max(1);
    let mut timings = Vec::with_capacity(repeats);
    let mut encoded = Vec::new();
    for _ in 0..repeats {
        let started = Instant::now();
        encoded = encode(codec, fixture, quality)?;
        timings.push(started.elapsed());
    }
    timings.sort_unstable();
    let median_encode = timings[timings.len() / 2];
    let decoded = decode_rgba(&encoded)?;
    if decoded.width != fixture.width || decoded.height != fixture.height {
        bail!(
            "{} returned {}x{}, expected {}x{}",
            codec,
            decoded.width,
            decoded.height,
            fixture.width,
            fixture.height
        );
    }
    if decoded.rgba[3..]
        .iter()
        .step_by(4)
        .any(|alpha| *alpha != 255)
    {
        bail!("{codec} decoded with non-opaque alpha");
    }
    let sampling = jpeg_sampling(&encoded)?;
    if sampling != Sampling::S444 {
        bail!("{codec} emitted {sampling} instead of required 4:4:4");
    }
    let (psnr_rgb, max_abs_rgb) = rgb_error(&fixture.rgba, &decoded.rgba);
    let delta_mae = neighbor_delta_mae(&fixture.rgba, &decoded.rgba, fixture.width);
    let seconds = median_encode.as_secs_f64();
    Ok(ProbeResult {
        codec,
        fixture: fixture.name,
        width: fixture.width,
        height: fixture.height,
        quality,
        encoded_bytes: encoded.len(),
        median_encode,
        megapixels_per_second: fixture.megapixels() / seconds,
        psnr_rgb,
        max_abs_rgb,
        delta_mae,
        sampling,
    })
}

struct Decoded {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn decode_rgba(jpeg: &[u8]) -> Result<Decoded> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(Cursor::new(jpeg), options);
    let rgba = decoder.decode().context("zune-jpeg cross-decode failed")?;
    let (width, height) = decoder.dimensions().context("JPEG has no dimensions")?;
    Ok(Decoded {
        width: u32::try_from(width)?,
        height: u32::try_from(height)?,
        rgba,
    })
}

fn rgb_error(source: &[u8], decoded: &[u8]) -> (f64, u8) {
    let mut squared_error = 0_f64;
    let mut max_abs = 0_u8;
    let mut samples = 0_u64;
    for (source_pixel, decoded_pixel) in source.chunks_exact(4).zip(decoded.chunks_exact(4)) {
        for channel in 0..3 {
            let error = i32::from(source_pixel[channel]) - i32::from(decoded_pixel[channel]);
            max_abs = max_abs.max(error.unsigned_abs() as u8);
            squared_error += f64::from(error * error);
            samples += 1;
        }
    }
    let mse = squared_error / samples as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0_f64.powi(2) / mse).log10()
    };
    (psnr, max_abs)
}

fn neighbor_delta_mae(source: &[u8], decoded: &[u8], width: u32) -> f64 {
    let width = width as usize;
    let mut total = 0_u64;
    let mut samples = 0_u64;
    for (source_row, decoded_row) in source
        .chunks_exact(width * 4)
        .zip(decoded.chunks_exact(width * 4))
    {
        for x in 1..width {
            let previous = (x - 1) * 4;
            let current = x * 4;
            for channel in 0..3 {
                let source_delta = i16::from(source_row[current + channel])
                    - i16::from(source_row[previous + channel]);
                let decoded_delta = i16::from(decoded_row[current + channel])
                    - i16::from(decoded_row[previous + channel]);
                total += u64::from((source_delta - decoded_delta).unsigned_abs());
                samples += 1;
            }
        }
    }
    total as f64 / samples.max(1) as f64
}

pub fn jpeg_sampling(bytes: &[u8]) -> Result<Sampling> {
    if bytes.len() < 4 || bytes[..2] != [0xff, 0xd8] {
        bail!("not a JPEG stream");
    }
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xff {
            bail!("invalid JPEG marker at offset {offset}");
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset).context("truncated JPEG marker")?;
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length_bytes = bytes
            .get(offset..offset + 2)
            .context("truncated JPEG segment length")?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 || offset + length > bytes.len() {
            bail!("invalid JPEG segment length");
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            let segment = &bytes[offset + 2..offset + length];
            if segment.len() < 9 {
                bail!("truncated JPEG frame header");
            }
            let components = usize::from(segment[5]);
            if components != 3 || segment.len() < 6 + components * 3 {
                return Ok(Sampling::Other);
            }
            let all_1x1 = (0..components).all(|component| segment[7 + component * 3] == 0x11);
            return Ok(if all_1x1 {
                Sampling::S444
            } else {
                Sampling::Other
            });
        }
        offset += length;
    }
    bail!("JPEG frame header not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_cross_decodes_as_opaque_444() {
        let fixture = synthetic_photo("small", 127, 93);
        for codec in Codec::ALL {
            let result = probe(codec, &fixture, 97, 1).unwrap();
            assert_eq!(result.sampling, Sampling::S444, "{codec}");
            assert!(result.psnr_rgb > 30.0, "{codec}: {}", result.psnr_rgb);
        }
    }

    #[test]
    fn invalid_fixture_is_rejected_before_ffi() {
        let fixture = Fixture {
            name: "invalid",
            width: 20,
            height: 20,
            rgba: vec![0; 7],
        };
        for codec in Codec::ALL {
            assert!(encode(codec, &fixture, 97).is_err(), "{codec}");
        }
    }

    #[test]
    fn invalid_quality_is_rejected_by_probe() {
        let fixture = synthetic_photo("small", 8, 8);
        assert!(probe(Codec::LibjpegTurboC, &fixture, 0, 1).is_err());
        assert!(probe(Codec::LibjpegTurboC, &fixture, 101, 1).is_err());
    }

    #[test]
    fn dedicated_rusturbo_pool_matches_automatic_output() {
        let fixture = synthetic_photo("small", 127, 93);
        let dedicated = DedicatedRusturboEncoder::new(2, 97).unwrap();
        assert_eq!(dedicated.workers(), 2);
        assert_eq!(
            dedicated.encode(&fixture).unwrap(),
            encode(
                Codec::JpegRusturboRows { threads: 0 },
                &fixture,
                97,
            )
            .unwrap()
        );
    }

    #[test]
    fn dedicated_rusturbo_pool_rejects_invalid_configuration() {
        assert!(DedicatedRusturboEncoder::new(0, 97).is_err());
        assert!(DedicatedRusturboEncoder::new(2, 0).is_err());
        assert!(DedicatedRusturboEncoder::new(2, 101).is_err());
    }
}
