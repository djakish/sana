//! Shared framed-object envelope.
//!
//! Every self-describing object Sana writes to the store — WAL batches, the
//! vector index, the vector version map — wraps its body in the same 20-byte
//! header, so a torn or corrupt write is always detectable:
//!
//! ```text
//! magic[8] | format_version: u32 LE | body_len: u32 LE | crc32(body): u32 LE | body
//! ```

use crate::error::{Error, Result};

pub const HEADER_LEN: usize = 8 + 4 + 4 + 4;

/// Wrap `body` in the envelope: magic, version, length, CRC32, then the body.
pub fn encode(magic: &[u8; 8], version: u32, body: &[u8]) -> Vec<u8> {
    let crc = crc32fast::hash(body);
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Validate the envelope of `bytes` and return its body slice. `what` names the
/// object in error messages (e.g. "wal", "vector index").
pub fn decode<'a>(bytes: &'a [u8], magic: &[u8; 8], version: u32, what: &str) -> Result<&'a [u8]> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Corrupt(format!("{what} frame shorter than header")));
    }
    if &bytes[0..8] != magic {
        return Err(Error::Corrupt(format!("bad {what} magic")));
    }
    let got = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed-size window"));
    if got != version {
        return Err(Error::Corrupt(format!("unsupported {what} version {got}")));
    }
    let body_len =
        u32::from_le_bytes(bytes[12..16].try_into().expect("fixed-size window")) as usize;
    let crc = u32::from_le_bytes(bytes[16..20].try_into().expect("fixed-size window"));
    let body = bytes
        .get(HEADER_LEN..HEADER_LEN + body_len)
        .ok_or_else(|| Error::Corrupt(format!("{what} body truncated")))?;
    if crc32fast::hash(body) != crc {
        return Err(Error::Corrupt(format!("{what} crc mismatch")));
    }
    Ok(body)
}
