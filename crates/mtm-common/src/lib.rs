//! Shared leaf types and config loading. Keep this crate dependency-light —
//! everything in the workspace depends on it.

pub mod config;
pub mod types;

pub use types::{Cluster, Symbol};
