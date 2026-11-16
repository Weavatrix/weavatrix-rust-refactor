//! Evidence-backed refactor operations for Weavatrix.
//!
//! This crate is to [`weavatrix_rust`] what a writer is to a reader: it consumes the read-only
//! evidence graph and produces `weavatrix.edit-plan.v1` envelopes, then applies them through
//! [`weavatrix_worktree`]'s crash-recoverable transaction. It owns no protocol; the MCP host
//! (`weavatrix-refactor`) composes this catalog with the read-only one.
//!
//! # The contract is frozen, not re-derived
//!
//! The tool names, their schemas and every result state live in
//! `contract/refactor-tools.v1.json`. Contract version 4 adds `extract_duplicate`,
//! `extract_window`, and `share_method_body`, and keeps the result states from version 3.
//! An operation here is conformant when it answers with a status from that file — never a new
//! one, never a renamed one.
//!
//! # Safety boundary
//!
//! Producing a plan is a read. Applying one requires all three gates: the host exposes the edit
//! capability, `WEAVATRIX_ALLOW_SOURCE_EDITS=1` is set, and the call presents a single-use token
//! bound to that exact plan and repository. Every call takes that repository's lock first:
//! readers share it, and a writer takes it alone. Any other repository runs at the same time.
//! A busy repository answers `REPO_BUSY` before a confirm token is spent. Nothing in this crate
//! writes source without the three gates.

pub mod contract;
pub mod coordinates;
pub mod declaration;
pub mod envelope;
pub mod evidence;
pub mod operations;
pub mod plan;
pub mod resolve;
pub mod specifier;
#[cfg(test)]
mod test_support;
pub mod token;

/// Version of this crate, reported by the host beside the engine version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the frozen tool contract this crate implements.
pub const CONTRACT_VERSION: u32 = 4;

pub use contract::{ResultState, ToolContract};
pub use operations::{Operation, call, catalog, catalog_names};
