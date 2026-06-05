//! Object-storage-native sorted string table. A generic immutable `bytes -> bytes`
//! key/value file with sorted keys; it backs document storage now and attribute
//! / FTS / vector-address families later.
//!
//! Layout:
//! ```text
//! [ data block | crc32 ]*          # prefix-compressed entries + restart array
//! [ index region ]                 # one entry per data block: last_key + handle
//! [ footer (fixed 32 bytes) ]      # index handle, format version, magic, crc
//! ```
//!
//! Stage 2 loads the whole object and parses in memory (one large round trip,
//! which the architecture prefers over many small ones). The format already
//! supports ranged reads — footer, then index, then only the needed block — as
//! a future optimization without changing on-disk bytes.

use bytes::Bytes;

use crate::error::{Error, Result};

const MAGIC: &[u8; 8] = b"SANASST1";
const FORMAT_VERSION: u32 = 1;
const FOOTER_LEN: usize = 8 + 8 + 4 + 4 + 8; // index_offset, index_size, index_crc, version, magic
const DEFAULT_BLOCK_TARGET: usize = 4096;
const DEFAULT_RESTART_INTERVAL: usize = 16;

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
    let mut shift = 0u32;
    loop {
        let byte = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("sst varint truncated".into()))?;
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Corrupt("sst varint overflow".into()));
        }
    }
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

    fn add(&mut self, key: &[u8], value: &[u8]) {
        let shared = if self.counter.is_multiple_of(self.restart_interval) {
            self.restarts.push(self.buf.len() as u32);
            0
        } else {
            common_prefix(&self.last_key, key)
        };
        let non_shared = key.len() - shared;
        put_uvarint(&mut self.buf, shared as u64);
        put_uvarint(&mut self.buf, non_shared as u64);
        put_uvarint(&mut self.buf, value.len() as u64);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.extend_from_slice(value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
    }

    fn is_empty(&self) -> bool {
        self.counter == 0
    }

    fn estimated_size(&self) -> usize {
        self.buf.len() + self.restarts.len() * 4 + 4
    }

    /// Append the restart array and return the finished block content.
    fn finish(self) -> Vec<u8> {
        let mut buf = self.buf;
        for r in &self.restarts {
            buf.extend_from_slice(&r.to_le_bytes());
        }
        buf.extend_from_slice(&(self.restarts.len() as u32).to_le_bytes());
        buf
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
        if let Some(last) = &self.last_added
            && key <= last.as_slice()
        {
            return Err(Error::Corrupt(
                "sst keys must be added in strictly increasing order".into(),
            ));
        }
        self.block.add(key, value);
        self.last_key_of_block.clear();
        self.last_key_of_block.extend_from_slice(key);
        self.last_added = Some(key.to_vec());
        if self.block.estimated_size() >= self.block_target {
            self.flush_block();
        }
        Ok(())
    }

    fn flush_block(&mut self) {
        let last_key = std::mem::take(&mut self.last_key_of_block);
        let content =
            std::mem::replace(&mut self.block, BlockBuilder::new(self.restart_interval)).finish();
        let offset = self.file.len() as u64;
        let size = content.len() as u64;
        let crc = crc32fast::hash(&content);
        self.file.extend_from_slice(&content);
        self.file.extend_from_slice(&crc.to_le_bytes());
        self.index.push((last_key, offset, size));
    }

    pub fn finish(mut self) -> Vec<u8> {
        if !self.block.is_empty() {
            self.flush_block();
        }
        let index_offset = self.file.len() as u64;
        let mut idx = Vec::new();
        idx.extend_from_slice(&(self.index.len() as u32).to_le_bytes());
        for (key, offset, size) in &self.index {
            idx.extend_from_slice(&(key.len() as u32).to_le_bytes());
            idx.extend_from_slice(key);
            idx.extend_from_slice(&offset.to_le_bytes());
            idx.extend_from_slice(&size.to_le_bytes());
        }
        let index_crc = crc32fast::hash(&idx);
        let index_size = idx.len() as u64;
        self.file.extend_from_slice(&idx);

        self.file.extend_from_slice(&index_offset.to_le_bytes());
        self.file.extend_from_slice(&index_size.to_le_bytes());
        self.file.extend_from_slice(&index_crc.to_le_bytes());
        self.file.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        self.file.extend_from_slice(MAGIC);
        self.file
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
        if n < FOOTER_LEN {
            return Err(Error::Corrupt("sst shorter than footer".into()));
        }
        let footer = &data[n - FOOTER_LEN..];
        if &footer[24..32] != MAGIC {
            return Err(Error::Corrupt("bad sst magic".into()));
        }
        let version = u32::from_le_bytes(
            footer[20..24]
                .try_into()
                .expect("slice is a fixed-size window"),
        );
        if version != FORMAT_VERSION {
            return Err(Error::Corrupt(format!("unsupported sst version {version}")));
        }
        let index_offset = u64::from_le_bytes(
            footer[0..8]
                .try_into()
                .expect("slice is a fixed-size window"),
        ) as usize;
        let index_size = u64::from_le_bytes(
            footer[8..16]
                .try_into()
                .expect("slice is a fixed-size window"),
        ) as usize;
        let index_crc = u32::from_le_bytes(
            footer[16..20]
                .try_into()
                .expect("slice is a fixed-size window"),
        );

        let idx = data
            .get(index_offset..index_offset + index_size)
            .ok_or_else(|| Error::Corrupt("sst index region out of bounds".into()))?;
        if crc32fast::hash(idx) != index_crc {
            return Err(Error::Corrupt("sst index crc mismatch".into()));
        }

        let mut pos = 0usize;
        let count = read_u32(idx, &mut pos)? as usize;
        let mut index = Vec::with_capacity(count);
        for _ in 0..count {
            let klen = read_u32(idx, &mut pos)? as usize;
            let key = idx
                .get(pos..pos + klen)
                .ok_or_else(|| Error::Corrupt("sst index key out of bounds".into()))?
                .to_vec();
            pos += klen;
            let offset = read_u64(idx, &mut pos)?;
            let size = read_u64(idx, &mut pos)?;
            index.push(IndexEntry {
                last_key: key,
                offset,
                size,
            });
        }
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
        let start = entry.offset as usize;
        let size = entry.size as usize;
        let content = self
            .data
            .get(start..start + size)
            .ok_or_else(|| Error::Corrupt("sst block out of bounds".into()))?;
        let crc_bytes = self
            .data
            .get(start + size..start + size + 4)
            .ok_or_else(|| Error::Corrupt("sst block crc out of bounds".into()))?;
        let crc = u32::from_le_bytes(crc_bytes.try_into().expect("slice is a fixed-size window"));
        if crc32fast::hash(content) != crc {
            return Err(Error::Corrupt("sst block crc mismatch".into()));
        }

        if content.len() < 4 {
            return Err(Error::Corrupt("sst block too small".into()));
        }
        let num_restarts = u32::from_le_bytes(
            content[content.len() - 4..]
                .try_into()
                .expect("slice is a fixed-size window"),
        ) as usize;
        let entries_end = content
            .len()
            .checked_sub(4 + num_restarts * 4)
            .ok_or_else(|| Error::Corrupt("sst block restart array out of bounds".into()))?;

        let mut out = Vec::new();
        let mut last_key: Vec<u8> = Vec::new();
        let mut pos = 0usize;
        while pos < entries_end {
            let shared = get_uvarint(content, &mut pos)? as usize;
            let non_shared = get_uvarint(content, &mut pos)? as usize;
            let value_len = get_uvarint(content, &mut pos)? as usize;
            if shared > last_key.len() || pos + non_shared > entries_end {
                return Err(Error::Corrupt("sst entry key out of bounds".into()));
            }
            let mut key = Vec::with_capacity(shared + non_shared);
            key.extend_from_slice(&last_key[..shared]);
            key.extend_from_slice(&content[pos..pos + non_shared]);
            pos += non_shared;
            let value_start = start + pos;
            if pos + value_len > entries_end {
                return Err(Error::Corrupt("sst entry value out of bounds".into()));
            }
            pos += value_len;
            let value = self.data.slice(value_start..value_start + value_len);
            last_key = key.clone();
            out.push((key, value));
        }
        Ok(out)
    }
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let b = buf
        .get(*pos..*pos + 4)
        .ok_or_else(|| Error::Corrupt("sst u32 out of bounds".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(
        b.try_into().expect("slice is a fixed-size window"),
    ))
}

fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let b = buf
        .get(*pos..*pos + 8)
        .ok_or_else(|| Error::Corrupt("sst u64 out of bounds".into()))?;
    *pos += 8;
    Ok(u64::from_le_bytes(
        b.try_into().expect("slice is a fixed-size window"),
    ))
}
