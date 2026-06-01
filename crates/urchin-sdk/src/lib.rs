//! Urchin SDK — two primitives only:
//! - Push: POST events to any urchin endpoint (local or cloud)
//! - Pull: GET paginated events from the cloud hub
//!
//! This crate must never grow to include: journal access, config parsing,
//! collector logic, peer management, or storage I/O. Those belong in their
//! respective crates. The SDK is the wire protocol adapter, nothing more.

pub mod builder;
pub mod client;

pub use builder::EventBuilder;
pub use client::{HttpError, UrchinClient};
