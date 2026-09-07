// SPDX-License-Identifier: MPL-2.0

//! Target parameter parsing for `DM_TABLE_LOAD`.
//!
//! This module centralizes the parsing of target-specific parameter strings
//! (as provided by userspace via the ioctl data area) into concrete
//! [`aster_dm::target::Target`] implementations. Previously these parsers
//! were inlined in `control.rs`; extracting them keeps the ioctl handler
//! focused on the ioctl protocol and lets the parsers be reused and tested
//! independently.

use aster_dm::{
    DmError, DmTarget,
    targets::{
        error::ErrorTarget, linear::LinearTarget, striped::StripedTarget, verity::VerityTarget,
        zero::ZeroTarget,
    },
};

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
    match target_type {
        "linear" => Ok(DmTarget::Linear(parse_linear_params(params)?)),
        "striped" => Ok(DmTarget::Striped(parse_striped_params(params)?)),
        "zero" => {
            if !params.split_whitespace().next().is_none() {
                return_errno_with_message!(Errno::EINVAL, "zero target takes no parameters");
            }
            Ok(DmTarget::Zero(ZeroTarget::new(len_sectors)))
        }
        "error" => {
            if !params.split_whitespace().next().is_none() {
                return_errno_with_message!(Errno::EINVAL, "error target takes no parameters");
            }
            Ok(DmTarget::Error(ErrorTarget::new(len_sectors)))
        }
        "verity" => Ok(DmTarget::Verity(Box::new(parse_verity_params(params)?))),
        _ => return_errno_with_message!(Errno::EINVAL, "unsupported target type"),
    }
}

/// Parses striped target parameters.
///
/// Format: `"<num_stripes> <stripe_size_sectors> <dev1> <start1> [<dev2> <start2> ...]"`
///
/// LVM uses the `striped` target for all logical volumes, even single-stripe
/// (linear) ones. A single-stripe striped target is semantically identical to
/// `linear`; multi-stripe targets distribute I/O across the underlying devices
/// in round-robin fashion (RAID0).
fn parse_striped_params(params: &str) -> Result<StripedTarget> {
    let parts: Vec<&str> = params.split_whitespace().collect();
    if parts.len() < 4 {
        return_errno_with_message!(
            Errno::EINVAL,
            "striped target requires at least: <num_stripes> <stripe_size> <dev> <start>"
        );
    }

    let num_stripes: usize = parts[0]
        .parse()
        .map_err(|_| Error::with_message(Errno::EINVAL, "invalid stripe count"))?;
    if num_stripes == 0 {
        return_errno_with_message!(Errno::EINVAL, "stripe count must be non-zero");
    }

    let stripe_size: u64 = parts[1]
        .parse()
        .map_err(|_| Error::with_message(Errno::EINVAL, "invalid stripe size"))?;
    if stripe_size == 0 {
        return_errno_with_message!(Errno::EINVAL, "stripe size must be non-zero");
    }

    // Expect exactly 2 + 2 * num_stripes fields: the count, the stripe size,
    // and one (device, start) pair per stripe.
    if parts.len() != 2 + 2 * num_stripes {
        return_errno_with_message!(
            Errno::EINVAL,
            "striped target has mismatched device/start pair count"
        );
    }

    let mut stripes = Vec::with_capacity(num_stripes);
    let mut idx = 2;
    for _ in 0..num_stripes {
        let device = aster_dm::lookup_block_device(parts[idx]).map_err(map_dm_error)?;
        let start: u64 = parts[idx + 1]
            .parse()
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid stripe start sector"))?;
        stripes.push((device, start));
        idx += 2;
    }

    Ok(StripedTarget::new(stripes, stripe_size))
}

/// Parses linear target parameters: `"major:minor start_sector"` or `"/dev/path start_sector"`.
fn parse_linear_params(params: &str) -> Result<LinearTarget> {
    let parts: Vec<&str> = params.split_whitespace().collect();
    if parts.len() < 2 {
        return_errno_with_message!(
            Errno::EINVAL,
            "linear target expects <device> <start_sector>"
        );
    }

    let dev_str = parts[0];
    let start_sector: u64 = parts[1]
        .parse()
        .map_err(|_| Error::with_message(Errno::EINVAL, "invalid start sector"))?;

    let device = aster_dm::lookup_block_device(dev_str).map_err(map_dm_error)?;
    Ok(LinearTarget::new(device, start_sector))
}

/// Parses verity target parameters.
///
/// Format:
/// ```text
/// <version> <data_dev> <hash_dev> <data_block_size> <hash_block_size>
/// <num_data_blocks> <hash_start_block> <algorithm> <root_digest> <salt> [0]
/// ```
fn parse_verity_params(params: &str) -> Result<VerityTarget> {
    let args: Vec<&str> = params.split_whitespace().collect();
    VerityTarget::from_table_args(&args).map_err(map_dm_error)
}

/// Maps a device-mapper component error to a kernel [`Error`].
///
/// `DmError::NotFound` becomes `ENOENT` (the referenced device does not exist),
/// while invalid parameters or malformed tables become `EINVAL`.
fn map_dm_error(err: DmError) -> Error {
    match err {
        DmError::InvalidParameters(msg) => Error::with_message(Errno::EINVAL, msg),
        DmError::InvalidTable(_) => {
            Error::with_message(Errno::EINVAL, "invalid device mapper table")
        }
        DmError::NotFound => Error::with_message(Errno::ENOENT, "underlying device not found"),
        _ => Error::with_message(Errno::EINVAL, "invalid device mapper parameters"),
    }
}
