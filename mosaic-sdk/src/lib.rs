//! Mosaic Agent SDK.
//!
//! High-level, idiomatic Rust API designed for AI agents (and humans writing
//! tooling) to drive Mosaic without learning every internal type. Every
//! agent has a stable `Identity`, every unit of work is an explicit `Session`
//! that closes into one atomic `Change`, and branches are first-class
//! "frontiers" you can speculate on cheaply.

pub mod agent;
pub mod session;
pub mod speculation;

pub use agent::MosaicAgent;
pub use session::Session;
pub use speculation::Speculation;

pub use mosaic_core::error::Error;
pub use mosaic_core::m1::change::{ChangeId, FileKind};
pub use mosaic_core::m1::identity::Identity;
pub use mosaic_core::m1::signing::SigningKey;
pub use mosaic_core::merge_strategies::FileMerge;
pub use mosaic_core::semantic::{SemanticHint, ThreeWaySemanticReport};
