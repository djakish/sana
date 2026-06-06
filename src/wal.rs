//! Write-ahead log batch format and codec. The WAL is the only synchronous
//! durable write path. Each batch is atomic: all operations become visible
//! together once committed.
//!
//! Wire format is a binary envelope (so a torn or corrupt write is detectable)
//! wrapping a `postcard`-encoded body:
//!
//! ```text
//! magic[8] = "SANAWAL1" | format_version: u32 LE | body_len: u32 LE |
//! crc32(body): u32 LE | body
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::frame;
use crate::value::{Document, Id, Value, VectorValue};

pub const WAL_MAGIC: &[u8; 8] = b"SANAWAL1";
pub const WAL_FORMAT_VERSION: u32 = 1;

/// A position in the WAL: writes advance `seq` within an `epoch`; an epoch bump
/// lets the log be rotated without resetting sequence ordering globally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WalCursor {
    pub epoch: u64,
    pub seq: u64,
}

impl WalCursor {
    pub fn new(epoch: u64, seq: u64) -> Self {
        Self { epoch, seq }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WalOp {
    Upsert {
        id: Id,
        document: Document,
    },
    Patch {
        id: Id,
        #[serde(default)]
        attributes: BTreeMap<String, Value>,
        #[serde(default)]
        vectors: BTreeMap<String, VectorValue>,
    },
    Delete {
        id: Id,
    },
}

/// One atomic write batch. `sequence` matches the `seq` of the [`WalCursor`] at
/// which it commits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WalBatch {
    pub namespace: String,
    pub sequence: u64,
    pub created_at_ms: u64,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub operations: Vec<WalOp>,
}

impl WalBatch {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))?;
        frame::encode(WAL_MAGIC, WAL_FORMAT_VERSION, &body)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body = frame::decode(bytes, WAL_MAGIC, WAL_FORMAT_VERSION, "wal")?;
        postcard::from_bytes(body).map_err(|e| Error::Codec(e.to_string()))
    }
}
