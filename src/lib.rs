//! Sana: an object-storage-native search database for vectors, text, and
//! attributes. See `docs/guide.md` for usage and `docs/PROGRESS.md` for the
//! staged build log and every design decision.

pub mod api;
pub mod attr;
pub mod backpressure;
pub mod cache_warm;
pub mod doc;
pub mod error;
pub mod frame;
pub mod index_queue;
pub mod indexer;
pub mod maintenance;
pub mod manifest;
pub mod metadata;
pub mod metrics;
pub mod namespace;
pub mod object_store;
pub mod operations;
pub mod pinning;
pub mod query;
pub mod queue_broker;
pub mod rabitq;
pub mod reader_lease;
pub mod schema;
pub mod sst;
pub mod text;
pub mod value;
pub mod vector;
pub mod wal;
pub mod write;

// Re-export the types most embedders touch, so a typical program imports from
// the crate root rather than reaching into individual modules.
pub use error::{Error, Result};
pub use namespace::Namespace;
pub use object_store::{FsObjectStore, ObjectStore};
pub use query::{FilterExpr, Query, RangeBound};
pub use value::{Document, Id, Value, VectorValue};
