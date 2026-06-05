//! Document-family encoding for the LSM: how an `Id` becomes a sortable SST key
//! and how a document (or its tombstone) becomes an SST value.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::value::{Document, Id};

const TAG_U64: u8 = 0;
const TAG_UUID: u8 = 1;
const TAG_STRING: u8 = 2;

/// Encode an `Id` into a key whose lexicographic byte order matches `Id`'s own
/// ordering: a leading tag groups variants (U64 < Uuid < String), then U64 uses
/// big-endian bytes (numeric order) and Uuid/String use their natural byte
/// order. Because a doc-family key is exactly one `Id`, the variable-length
/// string tail needs no terminator; composite keys (attribute family) will need
/// order-preserving framing, added when that family lands.
pub fn encode_id(id: &Id) -> Vec<u8> {
    match id {
        Id::U64(v) => {
            let mut b = Vec::with_capacity(9);
            b.push(TAG_U64);
            b.extend_from_slice(&v.to_be_bytes());
            b
        }
        Id::Uuid(u) => {
            let mut b = Vec::with_capacity(17);
            b.push(TAG_UUID);
            b.extend_from_slice(u);
            b
        }
        Id::String(s) => {
            let mut b = Vec::with_capacity(1 + s.len());
            b.push(TAG_STRING);
            b.extend_from_slice(s.as_bytes());
            b
        }
    }
}

pub fn decode_id(bytes: &[u8]) -> Result<Id> {
    match bytes.first() {
        Some(&TAG_U64) => {
            let arr: [u8; 8] = bytes
                .get(1..9)
                .ok_or_else(|| Error::Corrupt("doc id u64 truncated".into()))?
                .try_into()
                .expect("slice is a fixed-size window");
            Ok(Id::U64(u64::from_be_bytes(arr)))
        }
        Some(&TAG_UUID) => {
            let arr: [u8; 16] = bytes
                .get(1..17)
                .ok_or_else(|| Error::Corrupt("doc id uuid truncated".into()))?
                .try_into()
                .expect("slice is a fixed-size window");
            Ok(Id::Uuid(arr))
        }
        Some(&TAG_STRING) => {
            let s = std::str::from_utf8(&bytes[1..])
                .map_err(|_| Error::Corrupt("doc id string not utf-8".into()))?;
            Ok(Id::String(s.to_string()))
        }
        _ => Err(Error::Corrupt("unknown doc id tag".into())),
    }
}

/// An SST value in the document family: a live document or a tombstone. The
/// tombstone must survive in newer SSTs so a delete hides an older value until
/// compaction collapses the chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DocRecord {
    Present(Document),
    Deleted,
}

impl DocRecord {
    pub fn encode(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).map_err(|e| Error::Codec(e.to_string()))
    }
}
