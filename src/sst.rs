//! Object-storage-native sorted string table. A generic immutable `bytes -> bytes`
//! key/value file with sorted keys; it backs document storage now and attribute
//! / FTS / vector-address families later.
//!
//! Layout:
//! ```text
//! [ data block | crc32 ]*          # prefix-compressed entries + restart array
//! [ index region ]                 # one entry per data block: last_key + handle
//! [ footer (fixed 32/36 bytes) ]   # index handle, format version, magic, crc
//! ```
//!
//! Scans and the batch resolve path load the whole object and parse in memory
//! (one large round trip, which the architecture prefers over many small ones).
//! Single-key point lookups instead use [`ranged_get`], which reads only the
//! footer, the index, and the one candidate block — never the data region. Both
//! paths share the same footer/index/block decoders, so the format is described
//! once.

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::object_store::ObjectStore;

const MAGIC: &[u8; 8] = b"SANASST1";
const FORMAT_VERSION_V1: u32 = 1;
const FORMAT_VERSION: u32 = 2;
const FOOTER_V1_LEN: usize = 8 + 8 + 4 + 4 + 8;
const FOOTER_LEN: usize = 8 + 8 + 4 + 4 + 4 + 8;
const DEFAULT_BLOCK_TARGET: usize = 4096;
const DEFAULT_RESTART_INTERVAL: usize = 16;

fn checked_u32(value: usize, field: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| Error::InvalidWrite(format!("sst {field} exceeds u32 format limit")))
}

fn checked_u64(value: usize, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::InvalidWrite(format!("sst {field} exceeds u64 format limit")))
}

fn put_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            buf.push(b | 0x80);
        } else {
            buf.push(b);
            break;
        }
    }
}

fn get_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    for byte_index in 0..10 {
        let byte = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("sst varint truncated".into()))?;
        *pos += 1;
        if byte_index == 9 && byte > 1 {
            return Err(Error::Corrupt("sst varint overflow".into()));
        }
        result |= ((byte & 0x7f) as u64) << (byte_index * 7);
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(Error::Corrupt("sst varint overflow".into()))
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    restart_interval: usize,
    last_key: Vec<u8>,
}

impl BlockBuilder {
    fn new(restart_interval: usize) -> Self {
        Self {
            buf: Vec::new(),
            restarts: Vec::new(),
            counter: 0,
            restart_interval,
            last_key: Vec::new(),
        }
    }

    fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let shared = if self.counter.is_multiple_of(self.restart_interval) {
            self.restarts
                .push(checked_u32(self.buf.len(), "block restart offset")?);
            0
        } else {
            common_prefix(&self.last_key, key)
        };
        let non_shared = key.len() - shared;
        put_uvarint(&mut self.buf, checked_u64(shared, "shared key length")?);
        put_uvarint(
            &mut self.buf,
            checked_u64(non_shared, "unshared key length")?,
        );
        put_uvarint(&mut self.buf, checked_u64(value.len(), "value length")?);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.extend_from_slice(value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.counter == 0
    }

    fn estimated_size(&self) -> usize {
        self.buf
            .len()
            .saturating_add(self.restarts.len().saturating_mul(4))
            .saturating_add(4)
    }

    /// Append the restart array and return the finished block content.
    fn finish(self) -> Result<Vec<u8>> {
        let mut buf = self.buf;
        for r in &self.restarts {
            buf.extend_from_slice(&r.to_le_bytes());
        }
        buf.extend_from_slice(&checked_u32(self.restarts.len(), "restart count")?.to_le_bytes());
        Ok(buf)
    }
}

pub struct SstWriter {
    file: Vec<u8>,
    block: BlockBuilder,
    index: Vec<(Vec<u8>, u64, u64)>, // (last_key, offset, size)
    last_key_of_block: Vec<u8>,
    last_added: Option<Vec<u8>>,
    block_target: usize,
    restart_interval: usize,
}

impl Default for SstWriter {
    fn default() -> Self {
        Self::with_params(DEFAULT_BLOCK_TARGET, DEFAULT_RESTART_INTERVAL)
    }
}

impl SstWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_params(block_target: usize, restart_interval: usize) -> Self {
        let restart_interval = restart_interval.max(1);
        Self {
            file: Vec::new(),
            block: BlockBuilder::new(restart_interval),
            index: Vec::new(),
            last_key_of_block: Vec::new(),
            last_added: None,
            block_target,
            restart_interval,
        }
    }

    /// Add an entry. Keys must be added in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        checked_u32(key.len(), "index key length")?;
        if let Some(last) = &self.last_added
            && key <= last.as_slice()
        {
            return Err(Error::Corrupt(
                "sst keys must be added in strictly increasing order".into(),
            ));
        }
        self.block.add(key, value)?;
        self.last_key_of_block.clear();
        self.last_key_of_block.extend_from_slice(key);
        self.last_added = Some(key.to_vec());
        if self.block.estimated_size() >= self.block_target {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<()> {
        let last_key = std::mem::take(&mut self.last_key_of_block);
        let content = std::mem::replace(&mut self.block, BlockBuilder::new(self.restart_interval))
            .finish()?;
        let offset = checked_u64(self.file.len(), "file offset")?;
        let size = checked_u64(content.len(), "block size")?;
        let crc = crc32fast::hash(&content);
        self.file.extend_from_slice(&content);
        self.file.extend_from_slice(&crc.to_le_bytes());
        self.index.push((last_key, offset, size));
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8>> {
        if !self.block.is_empty() {
            self.flush_block()?;
        }
        let index_offset = checked_u64(self.file.len(), "index offset")?;
        let mut idx = Vec::new();
        idx.extend_from_slice(&checked_u32(self.index.len(), "index entry count")?.to_le_bytes());
        for (key, offset, size) in &self.index {
            idx.extend_from_slice(&checked_u32(key.len(), "index key length")?.to_le_bytes());
            idx.extend_from_slice(key);
            idx.extend_from_slice(&offset.to_le_bytes());
            idx.extend_from_slice(&size.to_le_bytes());
        }
        let index_crc = crc32fast::hash(&idx);
        let index_size = checked_u64(idx.len(), "index size")?;
        self.file.extend_from_slice(&idx);

        let mut footer = Vec::with_capacity(FOOTER_LEN);
        footer.extend_from_slice(&index_offset.to_le_bytes());
        footer.extend_from_slice(&index_size.to_le_bytes());
        footer.extend_from_slice(&index_crc.to_le_bytes());
        let mut footer_hasher = crc32fast::Hasher::new();
        footer_hasher.update(&footer);
        footer_hasher.update(&FORMAT_VERSION.to_le_bytes());
        footer_hasher.update(MAGIC);
        footer.extend_from_slice(&footer_hasher.finalize().to_le_bytes());
        footer.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        footer.extend_from_slice(MAGIC);
        debug_assert_eq!(footer.len(), FOOTER_LEN);
        self.file.extend_from_slice(&footer);
        Ok(self.file)
    }
}

struct IndexEntry {
    last_key: Vec<u8>,
    offset: u64,
    size: u64,
}

pub struct SstReader {
    data: Bytes,
    index: Vec<IndexEntry>,
}

impl SstReader {
    pub fn open(data: Bytes) -> Result<Self> {
        let n = data.len();
        if n < FOOTER_V1_LEN {
            return Err(Error::Corrupt("sst shorter than footer".into()));
        }
        let tail_start = n.saturating_sub(FOOTER_LEN);
        let footer = parse_footer(&data[tail_start..])?;
        validate_footer_layout(&footer, checked_u64(n, "object size")?)?;
        let start = usize::try_from(footer.index_offset)
            .map_err(|_| Error::Corrupt("sst index offset exceeds address space".into()))?;
        let index_size = usize::try_from(footer.index_size)
            .map_err(|_| Error::Corrupt("sst index size exceeds address space".into()))?;
        let end = start
            .checked_add(index_size)
            .ok_or_else(|| Error::Corrupt("sst index region overflow".into()))?;
        let idx = data
            .get(start..end)
            .ok_or_else(|| Error::Corrupt("sst index region out of bounds".into()))?;
        let index = parse_index(idx, footer.index_crc, footer.index_offset)?;
        Ok(Self { data, index })
    }

    /// Point lookup. Returns the value bytes (zero-copy slice) if present.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let bi = self.index.partition_point(|e| e.last_key.as_slice() < key);
        if bi >= self.index.len() {
            return Ok(None);
        }
        for (k, v) in self.decode_block(bi)? {
            if k.as_slice() == key {
                return Ok(Some(v));
            }
            if k.as_slice() > key {
                break;
            }
        }
        Ok(None)
    }

    /// All entries in sorted key order. Reads the whole file; used by merge and
    /// compaction.
    pub fn entries(&self) -> Result<Vec<(Vec<u8>, Bytes)>> {
        let mut out = Vec::new();
        for bi in 0..self.index.len() {
            out.append(&mut self.decode_block(bi)?);
        }
        Ok(out)
    }

    fn decode_block(&self, bi: usize) -> Result<Vec<(Vec<u8>, Bytes)>> {
        let entry = &self.index[bi];
        let start = usize::try_from(entry.offset)
            .map_err(|_| Error::Corrupt("sst block offset exceeds address space".into()))?;
        let size = usize::try_from(entry.size)
            .map_err(|_| Error::Corrupt("sst block size exceeds address space".into()))?;
        let end = start
            .checked_add(size)
            .and_then(|e| e.checked_add(4))
            .ok_or_else(|| Error::Corrupt("sst block region overflow".into()))?;
        if end > self.data.len() {
            return Err(Error::Corrupt("sst block out of bounds".into()));
        }
        let content = verify_block(&self.data.slice(start..end))?;
        decode_block_entries(&content)
    }
}

struct Footer {
    index_offset: u64,
    index_size: u64,
    index_crc: u32,
    len: usize,
}

/// Parse a trailing footer window. V1 has no footer checksum and remains
/// readable for compatibility; V2 checksums its index handle, index CRC,
/// version, and magic.
fn parse_footer(tail: &[u8]) -> Result<Footer> {
    if tail.len() < FOOTER_V1_LEN {
        return Err(Error::Corrupt("sst footer truncated".into()));
    }
    let magic_start = tail
        .len()
        .checked_sub(MAGIC.len())
        .ok_or_else(|| Error::Corrupt("sst footer truncated".into()))?;
    if &tail[magic_start..] != MAGIC {
        return Err(Error::Corrupt("bad sst magic".into()));
    }
    let version_start = magic_start
        .checked_sub(4)
        .ok_or_else(|| Error::Corrupt("sst footer version out of bounds".into()))?;
    let version = u32::from_le_bytes(
        tail[version_start..magic_start]
            .try_into()
            .expect("slice is a fixed-size window"),
    );
    let footer_len = match version {
        FORMAT_VERSION_V1 => FOOTER_V1_LEN,
        FORMAT_VERSION => FOOTER_LEN,
        _ => return Err(Error::Corrupt(format!("unsupported sst version {version}"))),
    };
    let footer_start = tail
        .len()
        .checked_sub(footer_len)
        .ok_or_else(|| Error::Corrupt("sst footer truncated".into()))?;
    let footer = &tail[footer_start..];

    if version == FORMAT_VERSION {
        let stored_crc = u32::from_le_bytes(
            footer[20..24]
                .try_into()
                .expect("slice is a fixed-size window"),
        );
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&footer[..20]);
        hasher.update(&footer[24..]);
        if hasher.finalize() != stored_crc {
            return Err(Error::Corrupt("sst footer crc mismatch".into()));
        }
    }

    Ok(Footer {
        index_offset: u64::from_le_bytes(
            footer[0..8]
                .try_into()
                .expect("slice is a fixed-size window"),
        ),
        index_size: u64::from_le_bytes(
            footer[8..16]
                .try_into()
                .expect("slice is a fixed-size window"),
        ),
        index_crc: u32::from_le_bytes(
            footer[16..20]
                .try_into()
                .expect("slice is a fixed-size window"),
        ),
        len: footer_len,
    })
}

fn validate_footer_layout(footer: &Footer, object_size: u64) -> Result<()> {
    let footer_len = u64::try_from(footer.len)
        .map_err(|_| Error::Corrupt("sst footer length exceeds u64".into()))?;
    let footer_start = object_size
        .checked_sub(footer_len)
        .ok_or_else(|| Error::Corrupt("sst footer outside object".into()))?;
    let index_end = footer
        .index_offset
        .checked_add(footer.index_size)
        .ok_or_else(|| Error::Corrupt("sst index region overflow".into()))?;
    if index_end != footer_start {
        return Err(Error::Corrupt(
            "sst index region does not end at footer".into(),
        ));
    }
    Ok(())
}

/// Verify the index region's CRC and decode it into per-block handles.
fn parse_index(idx: &[u8], index_crc: u32, data_end: u64) -> Result<Vec<IndexEntry>> {
    if crc32fast::hash(idx) != index_crc {
        return Err(Error::Corrupt("sst index crc mismatch".into()));
    }
    let mut pos = 0usize;
    let count = usize::try_from(read_u32(idx, &mut pos)?)
        .map_err(|_| Error::Corrupt("sst index entry count exceeds address space".into()))?;
    let max_entries = idx.len().saturating_sub(pos) / 20;
    if count > max_entries {
        return Err(Error::Corrupt("sst index entry count out of bounds".into()));
    }
    let mut index = Vec::with_capacity(count);
    let mut expected_offset = 0u64;
    for _ in 0..count {
        let klen = usize::try_from(read_u32(idx, &mut pos)?)
            .map_err(|_| Error::Corrupt("sst index key length exceeds address space".into()))?;
        let key_end = pos
            .checked_add(klen)
            .ok_or_else(|| Error::Corrupt("sst index key region overflow".into()))?;
        let key = idx
            .get(pos..key_end)
            .ok_or_else(|| Error::Corrupt("sst index key out of bounds".into()))?
            .to_vec();
        pos = key_end;
        let offset = read_u64(idx, &mut pos)?;
        let size = read_u64(idx, &mut pos)?;
        let block_end = offset
            .checked_add(size)
            .and_then(|end| end.checked_add(4))
            .ok_or_else(|| Error::Corrupt("sst block region overflow".into()))?;
        if offset != expected_offset || block_end > data_end {
            return Err(Error::Corrupt("sst block handle out of bounds".into()));
        }
        if index
            .last()
            .is_some_and(|previous: &IndexEntry| previous.last_key.as_slice() >= key.as_slice())
        {
            return Err(Error::Corrupt(
                "sst index keys are not strictly increasing".into(),
            ));
        }
        expected_offset = block_end;
        index.push(IndexEntry {
            last_key: key,
            offset,
            size,
        });
    }
    if pos != idx.len() {
        return Err(Error::Corrupt("sst index has trailing bytes".into()));
    }
    if expected_offset != data_end {
        return Err(Error::Corrupt(
            "sst index does not cover the data region".into(),
        ));
    }
    Ok(index)
}

/// Split a `content || crc32` block, verify the CRC, and return the content as a
/// zero-copy sub-slice. Shared by the whole-object reader and [`ranged_get`].
fn verify_block(block_with_crc: &Bytes) -> Result<Bytes> {
    let content_len = block_with_crc
        .len()
        .checked_sub(4)
        .ok_or_else(|| Error::Corrupt("sst block crc out of bounds".into()))?;
    let crc = u32::from_le_bytes(
        block_with_crc[content_len..]
            .try_into()
            .expect("slice is a fixed-size window"),
    );
    let content = block_with_crc.slice(0..content_len);
    if crc32fast::hash(&content) != crc {
        return Err(Error::Corrupt("sst block crc mismatch".into()));
    }
    Ok(content)
}

/// Decode every (key, value) pair from a verified block's content. Values are
/// zero-copy slices into `content`.
fn decode_block_entries(content: &Bytes) -> Result<Vec<(Vec<u8>, Bytes)>> {
    if content.len() < 4 {
        return Err(Error::Corrupt("sst block too small".into()));
    }
    let restart_count_start = content
        .len()
        .checked_sub(4)
        .ok_or_else(|| Error::Corrupt("sst block too small".into()))?;
    let num_restarts = usize::try_from(u32::from_le_bytes(
        content[restart_count_start..]
            .try_into()
            .expect("slice is a fixed-size window"),
    ))
    .map_err(|_| Error::Corrupt("sst restart count exceeds address space".into()))?;
    let restart_bytes = num_restarts
        .checked_mul(4)
        .ok_or_else(|| Error::Corrupt("sst block restart array overflow".into()))?;
    let trailer_len = restart_bytes
        .checked_add(4)
        .ok_or_else(|| Error::Corrupt("sst block restart trailer overflow".into()))?;
    let entries_end = content
        .len()
        .checked_sub(trailer_len)
        .ok_or_else(|| Error::Corrupt("sst block restart array out of bounds".into()))?;
    if num_restarts == 0 {
        return Err(Error::Corrupt("sst block has no restart points".into()));
    }
    let mut previous_restart = None;
    for restart_index in 0..num_restarts {
        let relative = restart_index
            .checked_mul(4)
            .ok_or_else(|| Error::Corrupt("sst restart offset overflow".into()))?;
        let start = entries_end
            .checked_add(relative)
            .ok_or_else(|| Error::Corrupt("sst restart offset overflow".into()))?;
        let end = start
            .checked_add(4)
            .ok_or_else(|| Error::Corrupt("sst restart offset overflow".into()))?;
        let restart = usize::try_from(u32::from_le_bytes(
            content
                .get(start..end)
                .ok_or_else(|| Error::Corrupt("sst restart offset out of bounds".into()))?
                .try_into()
                .expect("slice is a fixed-size window"),
        ))
        .map_err(|_| Error::Corrupt("sst restart offset exceeds address space".into()))?;
        if (restart_index == 0 && restart != 0)
            || restart >= entries_end
            || previous_restart.is_some_and(|previous| restart <= previous)
        {
            return Err(Error::Corrupt("invalid sst restart offsets".into()));
        }
        previous_restart = Some(restart);
    }

    let mut out = Vec::new();
    let mut last_key: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    while pos < entries_end {
        let shared = usize::try_from(get_uvarint(content, &mut pos)?)
            .map_err(|_| Error::Corrupt("sst shared key length exceeds address space".into()))?;
        let non_shared = usize::try_from(get_uvarint(content, &mut pos)?)
            .map_err(|_| Error::Corrupt("sst key length exceeds address space".into()))?;
        let value_len = usize::try_from(get_uvarint(content, &mut pos)?)
            .map_err(|_| Error::Corrupt("sst value length exceeds address space".into()))?;
        let key_end = pos
            .checked_add(non_shared)
            .ok_or_else(|| Error::Corrupt("sst entry key region overflow".into()))?;
        if shared > last_key.len() || key_end > entries_end {
            return Err(Error::Corrupt("sst entry key out of bounds".into()));
        }
        let key_len = shared
            .checked_add(non_shared)
            .ok_or_else(|| Error::Corrupt("sst reconstructed key length overflow".into()))?;
        let mut key = Vec::with_capacity(key_len);
        key.extend_from_slice(&last_key[..shared]);
        key.extend_from_slice(&content[pos..key_end]);
        pos = key_end;
        let value_start = pos;
        let value_end = pos
            .checked_add(value_len)
            .ok_or_else(|| Error::Corrupt("sst entry value region overflow".into()))?;
        if value_end > entries_end {
            return Err(Error::Corrupt("sst entry value out of bounds".into()));
        }
        pos = value_end;
        let value = content.slice(value_start..value_end);
        last_key = key.clone();
        out.push((key, value));
    }
    Ok(out)
}

/// Point lookup that reads only the bytes it needs from the object store: the
/// footer, the index region, then the one candidate block — never the
/// (potentially large) data region. `size` is the object length, which the
/// manifest records as `SstMeta::size_bytes`, so no extra `head` is needed.
///
/// This is the ranged read the format was designed for (architecture D16): three
/// small round trips instead of one whole-object GET. On object storage that
/// trades a couple of tiny requests for not transferring megabytes of data
/// blocks. The whole-object [`SstReader`] still backs scans and the batch
/// resolve path, where the full object is read anyway.
pub async fn ranged_get(
    store: &dyn ObjectStore,
    key: &str,
    size: u64,
    lookup_key: &[u8],
) -> Result<Option<Bytes>> {
    let minimum_footer_len = FOOTER_V1_LEN as u64;
    if size < minimum_footer_len {
        return Err(Error::Corrupt("sst shorter than footer".into()));
    }

    let tail_len = size.min(FOOTER_LEN as u64);
    let footer_bytes = store.get_range(key, size - tail_len..size).await?;
    let footer = parse_footer(&footer_bytes)?;
    validate_footer_layout(&footer, size)?;

    let index_end = footer
        .index_offset
        .checked_add(footer.index_size)
        .ok_or_else(|| Error::Corrupt("sst index region overflow".into()))?;
    let idx = store.get_range(key, footer.index_offset..index_end).await?;
    let index = parse_index(&idx, footer.index_crc, footer.index_offset)?;

    let bi = index.partition_point(|e| e.last_key.as_slice() < lookup_key);
    let Some(entry) = index.get(bi) else {
        return Ok(None);
    };
    let block_end = entry
        .offset
        .checked_add(entry.size)
        .and_then(|e| e.checked_add(4))
        .ok_or_else(|| Error::Corrupt("sst block region overflow".into()))?;
    let block = store.get_range(key, entry.offset..block_end).await?;
    let content = verify_block(&block)?;

    for (k, v) in decode_block_entries(&content)? {
        if k.as_slice() == lookup_key {
            return Ok(Some(v));
        }
        if k.as_slice() > lookup_key {
            break;
        }
    }
    Ok(None)
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let end = pos
        .checked_add(4)
        .ok_or_else(|| Error::Corrupt("sst u32 offset overflow".into()))?;
    let b = buf
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("sst u32 out of bounds".into()))?;
    *pos = end;
    Ok(u32::from_le_bytes(
        b.try_into().expect("slice is a fixed-size window"),
    ))
}

fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let end = pos
        .checked_add(8)
        .ok_or_else(|| Error::Corrupt("sst u64 offset overflow".into()))?;
    let b = buf
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("sst u64 out of bounds".into()))?;
    *pos = end;
    Ok(u64::from_le_bytes(
        b.try_into().expect("slice is a fixed-size window"),
    ))
}

#[cfg(test)]
mod tests {
    use super::checked_u32;
    use crate::error::Error;

    #[test]
    fn u32_format_fields_reject_oversized_values() {
        if usize::BITS > 32 {
            let oversized = (u32::MAX as usize)
                .checked_add(1)
                .expect("usize is wider than u32");
            assert!(matches!(
                checked_u32(oversized, "test field"),
                Err(Error::InvalidWrite(_))
            ));
        }
    }
}
