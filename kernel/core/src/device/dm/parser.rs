// SPDX-License-Identifier: MPL-2.0

//! Target parameter parsing for `DM_TABLE_LOAD`.
//!
//! The actual parsing logic lives in [`aster_dm::parse_target`], which is the
//! single source of truth shared with the boot-time `dm_mod.create=` path.
//! This module only:
//!
//! 1. Splits the `params` string (which follows the `DmTargetSpec` header in
//!    the ioctl data area) into whitespace-separated arguments;
//! 2. Delegates to [`aster_dm::parse_target`];
//! 3. Maps a device-mapper component error to a kernel [`Error`].

use aster_dm::{DmError, DmTarget};

use crate::prelude::*;

/// Parses a single target specification into a boxed [`Target`].
///
/// `target_type` is the target type name (e.g. `"linear"`, `"verity"`),
/// and `params` is the target-specific parameter string that follows the
/// `DmTargetSpec` header in the `DM_TABLE_LOAD` data area.
///
/// Supported target types: `linear`, `striped`, `zero`, `error`, `verity`.
///
/// `striped` is the target type LVM uses for logical volumes, even for
/// single-stripe (linear) LVs; a single-stripe striped target is equivalent
/// to `linear`.
pub fn parse_target(target_type: &str, params: &str, len_sectors: u64) -> Result<DmTarget> {
    let args: Vec<&str> = params.split_whitespace().collect();
    aster_dm::parse_target(target_type, &args, len_sectors).map_err(map_dm_error)
}

/// Maps a device-mapper component error to a kernel [`Error`].
///
/// `DmError::NotFound`/`DmError::ResolveBacking` become `ENOENT` (the
/// referenced backing device does not exist), while unsupported target
/// types, invalid parameters, or malformed tables become `EINVAL`.
fn map_dm_error(err: DmError) -> Error {
    match err {
        DmError::UnsupportedTarget => {
            Error::with_message(Errno::EINVAL, "unsupported device mapper target type")
        }
        DmError::Table(msg) => Error::with_message(Errno::EINVAL, msg),
        DmError::ResolveBacking(msg) => Error::with_message(Errno::ENOENT, msg),
        DmError::InvalidParameters(msg) => Error::with_message(Errno::EINVAL, msg),
        DmError::InvalidTable(_) => {
            Error::with_message(Errno::EINVAL, "invalid device mapper table")
        }
        DmError::NotFound => Error::with_message(Errno::ENOENT, "underlying device not found"),
        _ => Error::with_message(Errno::EINVAL, "invalid device mapper parameters"),
    }
}
