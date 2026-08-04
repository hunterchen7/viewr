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
///
/// Pixel storage is immutable outside `viewr-core`. This keeps the private
/// opaque-alpha provenance trustworthy after construction:
///
/// ```compile_fail
/// use viewr_core::types::PixelBuf;
///
/// let mut pixels = PixelBuf::new(1, 1, vec![0, 0, 0, 255]);
/// pixels.rgba[3] = 0;
/// ```
#[derive(Clone)]
pub struct PixelBuf {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Row-major RGBA bytes, with four bytes per pixel.
    ///
    /// Pipeline-produced buffers satisfy
    /// `rgba.len() == width * height * 4`. Callers of [`PixelBuf::new`] must
    /// preserve that invariant before passing the buffer to resize or JPEG
    /// functions.
    pub(crate) rgba: Vec<u8>,
    alpha: PixelAlpha,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelAlpha {
    Unknown,
    Opaque,
}

impl PixelBuf {
    /// Creates a buffer whose alpha provenance is unknown.
    ///
    /// This constructor deliberately does not scan or validate storage.
    /// Texture conversion therefore uses the exact premultiplying fallback,
    /// and malformed storage is handled by each consumer's existing checks.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            width,
            height,
            rgba,
            alpha: PixelAlpha::Unknown,
        }
    }

    /// Creates a known-opaque buffer after validating dimensions and alpha.
    ///
    /// This safe public entry point is intended for external producers. Core
    /// pipeline producers use their restricted proof-carrying constructor so
    /// they do not repeat a full-frame alpha scan.
    pub fn try_new_opaque(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected_len = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)?;
        if rgba.len() != expected_len || !rgba.chunks_exact(4).all(|pixel| pixel[3] == u8::MAX) {
            return None;
        }
        Some(Self {
            width,
            height,
            rgba,
            alpha: PixelAlpha::Opaque,
        })
    }

    /// Constructs storage whose producer proves it wrote opaque alpha.
    ///
    /// Restricted to `viewr-core`: every call site must be a producer or a
    /// transform that explicitly establishes or preserves alpha 255.
    pub(crate) fn new_opaque(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        debug_assert_eq!(
            usize::try_from(width)
                .ok()
                .and_then(|width| usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height)))
                .and_then(|pixels| pixels.checked_mul(4)),
            Some(rgba.len())
        );
        debug_assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == u8::MAX));
        Self {
            width,
            height,
            rgba,
            alpha: PixelAlpha::Opaque,
        }
    }

    /// Classifies decoder output without losing malformed or translucent data.
    ///
    /// Some decoders accept truncated streams and return partially initialized
    /// RGBA storage. Those buffers must remain usable through the exact alpha
    /// fallback, while valid opaque output carries its proof to the UI.
    pub(crate) fn new_scanned(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        let expected_len = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4));
        let alpha = if expected_len == Some(rgba.len())
            && rgba.chunks_exact(4).all(|pixel| pixel[3] == u8::MAX)
        {
            PixelAlpha::Opaque
        } else {
            PixelAlpha::Unknown
        };
        Self {
            width,
            height,
            rgba,
            alpha,
        }
    }

    /// Rebuilds a transformed buffer while retaining its alpha provenance.
    pub(crate) fn from_parts(width: u32, height: u32, rgba: Vec<u8>, alpha: PixelAlpha) -> Self {
        if alpha == PixelAlpha::Opaque {
            debug_assert_eq!(
                usize::try_from(width)
                    .ok()
                    .and_then(|width| usize::try_from(height)
                        .ok()
                        .and_then(|height| width.checked_mul(height)))
                    .and_then(|pixels| pixels.checked_mul(4)),
                Some(rgba.len())
            );
            debug_assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == u8::MAX));
        }
        Self {
            width,
            height,
            rgba,
            alpha,
        }
    }

    /// Borrows the tightly packed RGBA storage.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Consumes the buffer without copying its pixel allocation.
    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }

    /// Consumes the buffer into storage plus its non-forgeable provenance.
    pub(crate) fn into_parts(self) -> (Vec<u8>, PixelAlpha) {
        (self.rgba, self.alpha)
    }

    /// Returns whether a core producer proved every alpha byte is 255.
    pub fn is_opaque(&self) -> bool {
        self.alpha == PixelAlpha::Opaque
    }

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
        let buf = PixelBuf::new(2, 1, vec![1; 8]);
        assert_eq!(buf.byte_len(), 8);
        assert_eq!(
            format!("{buf:?}"),
            "PixelBuf { width: 2, height: 1, bytes: 8 }"
        );
    }

    #[test]
    fn opaque_provenance_is_validated_or_core_produced() {
        let unknown = PixelBuf::new(1, 1, vec![1, 2, 3, 255]);
        assert!(!unknown.is_opaque());

        let opaque =
            PixelBuf::try_new_opaque(1, 1, vec![1, 2, 3, 255]).expect("valid opaque pixel");
        assert!(opaque.is_opaque());
        assert!(opaque.clone().is_opaque());
        assert_eq!(opaque.rgba(), &[1, 2, 3, 255]);

        assert!(PixelBuf::try_new_opaque(1, 1, vec![1, 2, 3, 254]).is_none());
        assert!(PixelBuf::try_new_opaque(1, 1, vec![1, 2, 3]).is_none());
        assert!(PixelBuf::try_new_opaque(u32::MAX, u32::MAX, Vec::new()).is_none());

        assert!(PixelBuf::new_scanned(1, 1, vec![1, 2, 3, 255]).is_opaque());
        assert!(!PixelBuf::new_scanned(1, 1, vec![1, 2, 3, 0]).is_opaque());
        assert!(!PixelBuf::new_scanned(1, 1, vec![1, 2, 3]).is_opaque());
    }
}
