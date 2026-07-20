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
