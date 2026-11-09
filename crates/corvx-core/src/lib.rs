//! CORVX core: the state model shared by the daemon and anything embedding it.
//!
//! - `paths`: where CORVX keeps its data.
//! - `scan`: L1 structure scan of a project (stack, test and build commands, branch).
//! - `store`: SQLite persistence and the append-only event log.
//! - `service`: maps protocol methods onto store operations.

pub mod paths;
pub mod scan;
pub mod service;
pub mod store;

pub use service::{Core, ServiceError};
pub use store::{Store, StoreError};
