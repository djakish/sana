use crate::object_store::ObjectVersion;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("object not found: {0}")]
    NotFound(String),

    #[error("object already exists: {0}")]
    AlreadyExists(String),

    #[error("compare-and-set mismatch for {key}: expected {expected}, found {actual:?}")]
    CasMismatch {
        key: String,
        expected: ObjectVersion,
        actual: Option<ObjectVersion>,
    },

    #[error("invalid object range {start}..{end} for object of size {size}")]
    InvalidRange { start: u64, end: u64, size: u64 },

    #[error("corrupt data: {0}")]
    Corrupt(String),

    #[error("codec error: {0}")]
    Codec(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
