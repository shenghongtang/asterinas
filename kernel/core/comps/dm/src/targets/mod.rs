// SPDX-License-Identifier: MPL-2.0

//! Built-in device mapper target implementations.

pub mod error;
pub mod linear;
pub mod striped;
pub mod verity;
pub mod zero;

use crate::target::DmTargetMetadata;

/// All built-in target types supported by this kernel, sorted by name.
///
/// This static slice is the single source of truth for the target type
/// registry: `DM_LIST_VERSIONS` and `DM_GET_TARGET_VERSION` iterate it
/// directly. Adding a new target type only requires defining its `METADATA`
/// constant and listing it here (keeping the name-sorted order); no runtime
/// registration or init-order handling is needed.
pub const SUPPORTED_TARGETS: &[DmTargetMetadata] = &[
    error::METADATA,
    linear::METADATA,
    striped::METADATA,
    verity::METADATA,
    zero::METADATA,
];
