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
use crate::value::{Document, Id, Value, VectorValue};

pub const WAL_MAGIC: &[u8; 8] = b"SANAWAL1";
pub const WAL_FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 4 + 4;

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
        let crc = crc32fast::hash(&body);
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(WAL_MAGIC);
        out.extend_from_slice(&WAL_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Corrupt("wal frame shorter than header".into()));
        }
        if &bytes[0..8] != WAL_MAGIC {
            return Err(Error::Corrupt("bad wal magic".into()));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != WAL_FORMAT_VERSION {
            return Err(Error::Corrupt(format!("unsupported wal version {version}")));
        }
        let body_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let body = bytes
            .get(HEADER_LEN..HEADER_LEN + body_len)
            .ok_or_else(|| Error::Corrupt("wal body truncated".into()))?;
        if crc32fast::hash(body) != crc {
            return Err(Error::Corrupt("wal crc mismatch".into()));
        }
        postcard::from_bytes(body).map_err(|e| Error::Codec(e.to_string()))
    }
}
