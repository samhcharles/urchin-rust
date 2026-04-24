/// urchin-core: canonical event model, journal, identity, config.
/// All other crates depend on this. No I/O here — pure data types and logic.

pub mod event;
pub mod identity;
pub mod journal;
pub mod config;
