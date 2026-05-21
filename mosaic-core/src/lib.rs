//! Mosaic VCS core.
//!
//! L1 (Storage) and the primitives shared by every higher layer.
//! See `/root/.claude/plans/lexical-wibbling-whistle.md` for the full architecture.

pub mod chunker;
pub mod error;
pub mod hash;
pub mod storage;

pub use error::Error;
pub use hash::Hash;
pub use storage::{Cas, FsCas};
