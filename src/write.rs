//! Atomic known-ID conditional write request and response types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::query::FilterExpr;
use crate::value::{Id, Value, VectorValue};
use crate::wal::{WalCursor, WalOp};

pub use crate::backpressure::DEFAULT_MAX_UNINDEXED_WAL_BYTES;

pub const DEFAULT_PATCH_BY_FILTER_LIMIT: usize = 50_000;
pub const DEFAULT_DELETE_BY_FILTER_LIMIT: usize = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOptions {
    #[serde(default)]
    pub disable_backpressure: bool,
    #[serde(default = "default_max_unindexed_wal_bytes")]
    pub max_unindexed_wal_bytes: u64,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            disable_backpressure: false,
            max_unindexed_wal_bytes: DEFAULT_MAX_UNINDEXED_WAL_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConditionalWriteOp {
    pub operation: WalOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<FilterExpr>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOutcome {
    pub rows_affected: u64,
    pub rows_upserted: u64,
    pub rows_patched: u64,
    pub rows_deleted: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub rows_remaining: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionalWriteResult {
    pub cursor: WalCursor,
    pub outcome: WriteOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchByFilterRequest {
    pub filter: FilterExpr,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
    #[serde(default)]
    pub vectors: BTreeMap<String, VectorValue>,
    #[serde(default = "default_patch_limit")]
    pub max_rows: usize,
    #[serde(default)]
    pub allow_partial: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteByFilterRequest {
    pub filter: FilterExpr,
    #[serde(default = "default_delete_limit")]
    pub max_rows: usize,
    #[serde(default)]
    pub allow_partial: bool,
}

fn default_patch_limit() -> usize {
    DEFAULT_PATCH_BY_FILTER_LIMIT
}

fn default_delete_limit() -> usize {
    DEFAULT_DELETE_BY_FILTER_LIMIT
}

fn default_max_unindexed_wal_bytes() -> u64 {
    DEFAULT_MAX_UNINDEXED_WAL_BYTES
}

fn is_false(value: &bool) -> bool {
    !*value
}
