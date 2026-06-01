//! urchin-sleipnir: local distillation layer for the Urchin journal.
//!
//! Pipeline:
//!   journal (JSONL) -> cursor -> filter -> cluster -> store
//!
//! No LLM or network dependency. All processing is local and deterministic.

pub mod cluster;
pub mod cursor;
pub mod filter;
pub mod pipeline;
pub mod store;
