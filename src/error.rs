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

    #[error("invalid write: {0}")]
    InvalidWrite(String),

    #[error("invalid schema: {0}")]
    InvalidSchema(String),

    #[error("invalid query: {0}")]
    InvalidQuery(String),

    #[error("invalid indexing queue claim: {0}")]
    InvalidQueueClaim(String),

    #[error("corrupt data: {0}")]
    Corrupt(String),

    #[error("codec error: {0}")]
    Codec(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
