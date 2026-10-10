//! Shared plumbing for the per-venue DEX clients, orchestrators, and CLIs.
//!
//! - [`spl`]: pure SPL Token / ATA / System instruction builders and decoders.
//! - [`funding`]: typed test-token funding used by orchestrator libraries.
//! - `cli` (feature `cli`): Doppler-injected keypairs, shared clap argument
//!   groups, and the ephemeral-user lifecycle used by the orchestrator binaries.

#[cfg(feature = "cli")]
pub mod cli;
pub mod funding;
pub mod spl;
