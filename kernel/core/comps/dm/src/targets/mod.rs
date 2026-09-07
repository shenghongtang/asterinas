// SPDX-License-Identifier: MPL-2.0

//! Built-in device mapper target implementations.

pub mod error;
pub mod linear;
pub mod striped;
pub mod verity;
pub mod zero;

/// Registers all built-in target types and their versions with the global
/// target registry.
///
/// This must be called before any user-space or boot-time DM command attempts
/// to query supported target types (e.g. via `DM_LIST_VERSIONS`).
pub fn register_all() {
    linear::register();
    striped::register();
    zero::register();
    error::register();
    verity::register();
}
