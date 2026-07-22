//! Shared value types passed between decoding, caching, scheduling, and the UI.

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
    /// No display rotation.
    #[default]
    R0,
    /// 90° clockwise.
    R90,
    /// 180° clockwise.
    R180,
    /// 270° clockwise.
    R270,
}

impl Orient {
    /// Converts an EXIF orientation value into the supported display rotation.
    ///
    /// Mirrored EXIF values (`2`, `4`, `5`, and `7`) intentionally collapse to
    /// their nearest rotation. Missing and unknown values are treated as
    /// [`R0`](Self::R0).
    pub fn from_exif(value: Option<u16>) -> Self {
        match value {
            Some(3 | 4) => Self::R180,
            Some(5 | 6) => Self::R90,
            Some(7 | 8) => Self::R270,
            _ => Self::R0,
        }
    }

    /// Returns whether applying this orientation exchanges width and height.
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
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Row-major RGBA bytes, with four bytes per pixel.
    ///
    /// Pipeline-produced buffers satisfy
    /// `rgba.len() == width * height * 4`. Public callers constructing the
    /// struct directly must preserve that invariant before passing it to
    /// resize or JPEG functions.
    pub rgba: Vec<u8>,
}

impl PixelBuf {
    /// Returns the resident byte length of the pixel storage.
    ///
    /// This reports the actual vector length; it does not recompute or validate
    /// the expected length from the dimensions.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_orientation_mapping_covers_all_standard_values() {
        let cases = [
            (None, Orient::R0),
            (Some(1), Orient::R0),
            (Some(2), Orient::R0),
            (Some(3), Orient::R180),
            (Some(4), Orient::R180),
            (Some(5), Orient::R90),
            (Some(6), Orient::R90),
            (Some(7), Orient::R270),
            (Some(8), Orient::R270),
            (Some(9), Orient::R0),
        ];
        for (value, expected) in cases {
            assert_eq!(Orient::from_exif(value), expected, "EXIF {value:?}");
        }
    }

    #[test]
    fn only_quarter_turns_swap_axes() {
        assert!(!Orient::R0.swaps_axes());
        assert!(Orient::R90.swaps_axes());
        assert!(!Orient::R180.swaps_axes());
        assert!(Orient::R270.swaps_axes());
    }

    #[test]
    fn pixel_buffer_reports_storage_without_dumping_pixels() {
        let buf = PixelBuf {
            width: 2,
            height: 1,
            rgba: vec![1; 8],
        };
        assert_eq!(buf.byte_len(), 8);
        assert_eq!(
            format!("{buf:?}"),
            "PixelBuf { width: 2, height: 1, bytes: 8 }"
        );
    }
}
