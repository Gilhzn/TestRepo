//! Mosaic VCS core.
//!
//! L1 (Storage) and the primitives shared by every higher layer.
//! See `/root/.claude/plans/lexical-wibbling-whistle.md` for the full architecture.

pub mod ast;
pub mod chunker;
pub mod crdt;
pub mod error;
pub mod hash;
pub mod import_git;
pub mod m1;
pub mod m1_dag;
pub mod m1_patch;
pub mod merge_strategies;
pub mod repo;
pub mod review;
pub mod semantic;
pub mod storage;
pub mod storage_remote;
pub mod sync;
pub mod attestation;
pub mod working_copy;
pub mod gc;
pub mod audit;

pub use error::Error;
pub use hash::Hash;
pub use storage::{Cas, FsCas};
