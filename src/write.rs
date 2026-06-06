//! Atomic known-ID conditional write request and response types.

use serde::{Deserialize, Serialize};

use crate::query::FilterExpr;
use crate::value::Id;
use crate::wal::{WalCursor, WalOp};

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionalWriteResult {
    pub cursor: WalCursor,
    pub outcome: WriteOutcome,
}
