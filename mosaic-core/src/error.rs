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

    #[error("invalid identity: {0}")]
    InvalidIdentity(&'static str),

    #[error("invalid key length: expected {expected}, got {actual}")]
    InvalidKeyLength { expected: usize, actual: usize },

    #[error("invalid signature length: expected {expected}, got {actual}")]
    InvalidSignatureLength { expected: usize, actual: usize },

    #[error("signature verification failed")]
    BadSignature,

    #[error("serialization: {0}")]
    Serialization(String),

    #[error("unknown parent change: {0}")]
    UnknownParent(String),

    #[error("invalid ref name: {0}")]
    InvalidRefName(String),

    #[error("ref not found: {0}")]
    RefNotFound(String),

    #[error("invalid frontier line: {0}")]
    InvalidFrontierLine(String),

    #[error("invalid patch: {0}")]
    InvalidPatch(String),

    #[error("patch target missing: {0}")]
    PatchTargetMissing(String),

    #[error("line graph inconsistent: {0}")]
    LineGraphInconsistent(String),
}

pub type Result<T> = std::result::Result<T, Error>;
