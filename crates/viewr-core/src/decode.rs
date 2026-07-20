//! rawler wrappers: file → RawImage + metadata. Implemented in the M0 spike.

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rawler: {0}")]
    Rawler(String),
}
