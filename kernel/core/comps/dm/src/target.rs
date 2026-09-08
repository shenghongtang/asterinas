// SPDX-License-Identifier: MPL-2.0

//! The device mapper target trait.
//!
//! A target defines how I/O requests for a region of the mapped device are
//! remapped to one or more underlying block devices. The simplest target is
//! [`LinearTarget`](crate::targets::linear::LinearTarget), which maps a
//! contiguous logical range to a contiguous physical range on a single
//! underlying device.

use alloc::{sync::Arc, vec::Vec};

use aster_block::{
    BlockDevice, BlockDeviceMeta,
    bio::{BioEnqueueError, SubmittedBio},
};
use device_id::DeviceId;

use crate::{
    DmError,
    targets::{
        error::ErrorTarget, linear::LinearTarget, striped::StripedTarget, verity::VerityTarget,
        zero::ZeroTarget,
    },
};

/// A device mapper target.
///
/// Each target is responsible for remapping I/O requests that fall within its
/// region of the mapped device to the underlying block device(s).
///
/// The `logical_start` parameter passed to [`map_bio`](Target::map_bio) is the
/// starting sector of this target's region on the mapped device. Targets use
/// this to compute the appropriate sector offset for the underlying device.
pub trait Target: Send + Sync + core::fmt::Debug {
    /// Returns the name of the target type (e.g., `"linear"`).
    fn name(&self) -> &str;

    /// Maps and submits a bio that falls entirely within this target's region.
    ///
    /// `logical_start` is the start sector of this target on the mapped device.
    /// The bio's `sid_range` is guaranteed to be entirely within
    /// `[logical_start, logical_start + num_sectors)`.
    ///
    /// The target is responsible for adjusting the bio's sector offset (via
    /// [`SubmittedBio::set_sid_offset`]) and forwarding it to the underlying
    /// device's `enqueue` method.
    fn map_bio(&self, bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError>;

    /// Returns the metadata of the underlying device(s).
    ///
    /// The `nr_sectors` field is not used by the table (the table tracks
    /// target lengths explicitly); only `max_nr_segments_per_bio` is
    /// consulted to compute the mapped device's overall segment limit.
    fn metadata(&self) -> BlockDeviceMeta;

    /// Returns the parameters string for this target, used in `DM_TABLE_STATUS`
    /// and for diagnostics.
    ///
    /// The format is target-type specific. For example, a linear target
    /// returns `"major:minor start_sector"`.
    fn params(&self) -> &str;

    /// Returns the list of underlying block device IDs that this target
    /// depends on, used for `DM_TABLE_DEPS`.
    ///
    /// For example, a linear target returns the single underlying device's ID.
    /// More complex targets (e.g., striped) may return multiple device IDs.
    fn deps(&self) -> &[DeviceId];

    /// Returns the underlying block devices used for flush propagation.
    ///
    /// Device mapper flushes must reach every unique underlying device that
    /// backs the mapped device. This method returns the live `Arc<dyn BlockDevice>`
    /// handles so the table can issue flushes directly without requiring a
    /// fresh registry lookup.
    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>>;

    /// Handles a `DM_TARGET_MSG` request sent to this target.
    ///
    /// `sector` identifies the target region the message is addressed to
    /// (callers pass the start sector of the target). `message` is the
    /// null-terminated message string from userspace.
    ///
    /// The default implementation returns
    /// [`DmError::InvalidParameters`] with a generic "not supported" message.
    /// Targets that support messages (e.g. `verity`'s `create_device`) should
    /// override this.
    fn message(&self, _sector: u64, _message: &str) -> Result<(), DmError> {
        Err(DmError::InvalidParameters("target message not supported"))
    }
}

/// An enum wrapper around all built-in device mapper target implementations.
///
/// Using an enum instead of `Box<dyn Target>` removes the vtable indirection
/// for the hot `map_bio` path. Adding a new target type requires a new
/// variant here and matching it in the [`Target`] implementation below.
#[derive(Debug)]
pub enum DmTarget {
    /// A linear mapping target.
    Linear(LinearTarget),
    /// A striped (RAID0) mapping target.
    Striped(StripedTarget),
    /// A verity integrity-verification target.
    ///
    /// Held in an `Arc` so that per-bio async verification state can keep the
    /// target alive while chained data/hash-block reads are in flight.
    Verity(Arc<VerityTarget>),
    /// A target that returns zeroes.
    Zero(ZeroTarget),
    /// A target that always returns I/O errors.
    Error(ErrorTarget),
}

impl Target for DmTarget {
    fn name(&self) -> &str {
        match self {
            DmTarget::Linear(t) => t.name(),
            DmTarget::Striped(t) => t.name(),
            DmTarget::Verity(t) => t.name(),
            DmTarget::Zero(t) => t.name(),
            DmTarget::Error(t) => t.name(),
        }
    }

    fn map_bio(&self, bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError> {
        match self {
            DmTarget::Linear(t) => t.map_bio(bio, logical_start),
            DmTarget::Striped(t) => t.map_bio(bio, logical_start),
            DmTarget::Verity(t) => t.map_bio(bio, logical_start),
            DmTarget::Zero(t) => t.map_bio(bio, logical_start),
            DmTarget::Error(t) => t.map_bio(bio, logical_start),
        }
    }

    fn metadata(&self) -> BlockDeviceMeta {
        match self {
            DmTarget::Linear(t) => t.metadata(),
            DmTarget::Striped(t) => t.metadata(),
            DmTarget::Verity(t) => t.metadata(),
            DmTarget::Zero(t) => t.metadata(),
            DmTarget::Error(t) => t.metadata(),
        }
    }

    fn params(&self) -> &str {
        match self {
            DmTarget::Linear(t) => t.params(),
            DmTarget::Striped(t) => t.params(),
            DmTarget::Verity(t) => t.params(),
            DmTarget::Zero(t) => t.params(),
            DmTarget::Error(t) => t.params(),
        }
    }

    fn deps(&self) -> &[DeviceId] {
        match self {
            DmTarget::Linear(t) => t.deps(),
            DmTarget::Striped(t) => t.deps(),
            DmTarget::Verity(t) => t.deps(),
            DmTarget::Zero(t) => t.deps(),
            DmTarget::Error(t) => t.deps(),
        }
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        match self {
            DmTarget::Linear(t) => t.underlying_devices(),
            DmTarget::Striped(t) => t.underlying_devices(),
            DmTarget::Verity(t) => t.underlying_devices(),
            DmTarget::Zero(t) => t.underlying_devices(),
            DmTarget::Error(t) => t.underlying_devices(),
        }
    }

    fn message(&self, sector: u64, message: &str) -> Result<(), DmError> {
        match self {
            DmTarget::Linear(t) => t.message(sector, message),
            DmTarget::Striped(t) => t.message(sector, message),
            DmTarget::Verity(t) => t.message(sector, message),
            DmTarget::Zero(t) => t.message(sector, message),
            DmTarget::Error(t) => t.message(sector, message),
        }
    }
}
