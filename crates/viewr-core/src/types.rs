/// Cache/display tier of a rendered image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// Small filmstrip/grid thumbnail (from the embedded preview).
    Thumb,
    /// Half-res superpixel develop — real raw data, browse quality.
    Browse,
    /// Full-res PPG develop for 100% zoom.
    Full,
}

/// Display rotation derived from the EXIF orientation tag. Mirrored
/// variants collapse onto the nearest rotation (cameras don't mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orient {
    #[default]
    R0,
    /// 90° clockwise
    R90,
    R180,
    /// 270° clockwise
    R270,
}

impl Orient {
    pub fn from_exif(value: Option<u16>) -> Self {
        match value {
            Some(3 | 4) => Self::R180,
            Some(5 | 6) => Self::R90,
            Some(7 | 8) => Self::R270,
            _ => Self::R0,
        }
    }

    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::R90 | Self::R270)
    }
}

/// A decoded, display-ready image: 8-bit sRGB, tightly packed RGBA.
///
/// The only pixel type that crosses thread boundaries. UI-side texture types
/// are constructed from this on the UI thread only.
#[derive(Clone)]
pub struct PixelBuf {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl PixelBuf {
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

impl std::fmt::Debug for PixelBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PixelBuf")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}
