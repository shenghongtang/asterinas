// SPDX-License-Identifier: MPL-2.0

//! The linear target.
//!
//! A [`LinearTarget`] maps a contiguous range of logical sectors on the
//! mapped device to a contiguous range of physical sectors on a single
//! underlying block device, starting at a given offset.
//!
//! This is the simplest device mapper target and is the building block for
//! LVM logical volumes and simple concatenation.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::fmt;

use aster_block::{
    BlockDevice, BlockDeviceLease, BlockDeviceMeta,
    bio::{BioEnqueueError, SubmittedBio},
};
use device_id::DeviceId;

use crate::target::Target;

/// A linear mapping target.
///
/// Maps logical sector `L` on the mapped device to physical sector
/// `physical_start + (L - logical_start)` on the underlying device, where
/// `logical_start` is provided by the table entry at map time.
///
/// The target holds a [`BlockDeviceLease`] on the underlying device for its
/// entire lifetime, preventing the device from being unregistered while it
/// is still mapped. If the device was not registered (e.g., in kernel
/// tests), the lease is `None` and no lifecycle protection is provided.
pub struct LinearTarget {
    /// The underlying block device (used for I/O).
    device: Arc<dyn BlockDevice>,
    /// The device ID of the underlying device.
    device_id: DeviceId,
    /// The starting physical sector on the underlying device.
    physical_start: u64,
    /// Pre-formatted parameters string for `DM_TABLE_STATUS`.
    params: String,
    /// Lease preventing the underlying device from being unregistered.
    /// `None` only when the device is not in the block registry (tests).
    _lease: Option<BlockDeviceLease>,
}

/// Registers the `linear` target type and its version.
pub fn register() {
    // Matches Linux's dm-linear version (changes are internal optimizations,
    // no user-visible feature gating tied to the version number).
    crate::register_target_type("linear", [1, 14, 0]);
}

impl LinearTarget {
    /// Creates a new linear target.
    ///
    /// `physical_start` is the offset (in sectors) on the underlying device
    /// where this target's region begins.
    ///
    /// If the device is registered in the block registry, a lease is acquired
    /// automatically. This prevents the underlying device from being
    /// unregistered while this target exists.
    pub fn new(device: Arc<dyn BlockDevice>, physical_start: u64) -> Self {
        let lease = aster_block::lookup_lease(device.id());
        let device = lease.as_ref().map(|l| l.device().clone()).unwrap_or(device);
        let device_id = device.id();
        let params = format!(
            "{}:{} {}",
            device_id.major().get(),
            device_id.minor().get(),
            physical_start
        );
        Self {
            device,
            device_id,
            physical_start,
            params,
            _lease: lease,
        }
    }

    /// Returns the underlying block device.
    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.device
    }

    /// Returns the physical start sector on the underlying device.
    pub fn physical_start(&self) -> u64 {
        self.physical_start
    }
}

impl Target for LinearTarget {
    fn name(&self) -> &str {
        "linear"
    }

    fn map_bio(&self, mut bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError> {
        // physical_sector = physical_start + (bio_logical_sector - logical_start)
        //                  = bio_logical_sector + (physical_start - logical_start)
        // So sid_offset = physical_start - logical_start (may be negative).
        //
        // Accumulate with any existing offset (e.g., when stacking DM devices
        // like dm-mylv → testlv → /dev/vde). Overwriting would discard the
        // offset set by the outer DM device and cause writes to land on the
        // wrong sectors.
        let delta = if self.physical_start >= logical_start {
            let diff = self.physical_start - logical_start;
            i64::try_from(diff).map_err(|_| BioEnqueueError::Refused)?
        } else {
            let diff = logical_start - self.physical_start;
            let d = i64::try_from(diff).map_err(|_| BioEnqueueError::Refused)?;
            -d
        };
        let new_offset = bio
            .sid_offset()
            .checked_add(delta)
            .ok_or(BioEnqueueError::Refused)?;
        bio.set_sid_offset(new_offset);
        self.device.enqueue(bio)
    }

    fn metadata(&self) -> BlockDeviceMeta {
        self.device.metadata()
    }

    fn params(&self) -> &str {
        &self.params
    }

    fn deps(&self) -> &[DeviceId] {
        core::slice::from_ref(&self.device_id)
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        Vec::from([self.device.clone()])
    }
}

impl fmt::Debug for LinearTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LinearTarget")
            .field("device_id", &self.device_id)
            .field("physical_start", &self.physical_start)
            .finish()
    }
}
