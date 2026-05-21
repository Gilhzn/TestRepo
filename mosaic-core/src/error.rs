use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("hex decode: {0}")]
    Hex(#[from] hex::FromHexError),

    #[error("invalid hash length: {0}")]
    InvalidHashLength(usize),
}

pub type Result<T> = std::result::Result<T, Error>;
