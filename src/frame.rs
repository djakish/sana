//! Shared framed-object envelope.
//!
//! Every self-describing object Sana writes to the store — WAL batches, the
//! vector index, the vector version map — wraps its body in the same 20-byte
//! header, so a torn or corrupt write is always detectable:
//!
//! ```text
//! magic[8] | format_version: u32 LE | body_len: u32 LE | crc32(body): u32 LE | body
//! ```

use std::ops::Range;

use crate::error::{Error, Result};

pub const HEADER_LEN: usize = 8 + 4 + 4 + 4;

fn fixed<const N: usize>(
    bytes: &[u8],
    range: Range<usize>,
    what: &str,
    field: &str,
) -> Result<[u8; N]> {
    let start = range.start;
    let end = range.end;
    let window = bytes
        .get(range)
        .ok_or_else(|| Error::Corrupt(format!("{what} {field} out of bounds ({start}..{end})")))?;
    window
        .try_into()
        .map_err(|error| Error::Corrupt(format!("{what} {field} has invalid length: {error}")))
}

/// Wrap `body` in the envelope: magic, version, length, CRC32, then the body.
pub fn encode(magic: &[u8; 8], version: u32, body: &[u8]) -> Result<Vec<u8>> {
    let body_len = u32::try_from(body.len()).map_err(|error| {
        Error::InvalidWrite(format!(
            "framed object body exceeds the u32 format limit: {error}"
        ))
    })?;
    let capacity = HEADER_LEN
        .checked_add(body.len())
        .ok_or_else(|| Error::InvalidWrite("framed object size overflow".into()))?;
    let crc = crc32fast::hash(body);
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(magic);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Validate the envelope of `bytes` and return its body slice. `what` names the
/// object in error messages (e.g. "wal", "vector index").
pub fn decode<'a>(bytes: &'a [u8], magic: &[u8; 8], version: u32, what: &str) -> Result<&'a [u8]> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Corrupt(format!("{what} frame shorter than header")));
    }
    if fixed::<8>(bytes, 0..8, what, "magic")? != *magic {
        return Err(Error::Corrupt(format!("bad {what} magic")));
    }
    let got = u32::from_le_bytes(fixed(bytes, 8..12, what, "version")?);
    if got != version {
        return Err(Error::Corrupt(format!("unsupported {what} version {got}")));
    }
    let body_len = usize::try_from(u32::from_le_bytes(fixed(
        bytes,
        12..16,
        what,
        "body length",
    )?))
    .map_err(|error| Error::Corrupt(format!("{what} body length exceeds usize: {error}")))?;
    let crc = u32::from_le_bytes(fixed(bytes, 16..20, what, "crc")?);
    let body_end = HEADER_LEN
        .checked_add(body_len)
        .ok_or_else(|| Error::Corrupt(format!("{what} body length overflow")))?;
    if body_end != bytes.len() {
        return Err(Error::Corrupt(format!(
            "{what} frame length does not match its body length"
        )));
    }
    let body = bytes
        .get(HEADER_LEN..body_end)
        .ok_or_else(|| Error::Corrupt(format!("{what} body truncated")))?;
    if crc32fast::hash(body) != crc {
        return Err(Error::Corrupt(format!("{what} crc mismatch")));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};
    use crate::error::Error;

    const MAGIC: &[u8; 8] = b"TESTFRM1";

    #[test]
    fn frame_round_trips_and_rejects_trailing_bytes() {
        let encoded = encode(MAGIC, 1, b"body").unwrap();
        assert_eq!(decode(&encoded, MAGIC, 1, "test").unwrap(), b"body");

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            decode(&trailing, MAGIC, 1, "test"),
            Err(Error::Corrupt(_))
        ));
    }
}
