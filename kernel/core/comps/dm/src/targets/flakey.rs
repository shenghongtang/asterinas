// SPDX-License-Identifier: MPL-2.0

//! The `flakey` target.
//!
//! A [`FlakeyTarget`] passes I/O through to an underlying device during
//! periodic "up" intervals and injects failures during "down" intervals,
//! simulating an unreliable device for testing error-handling paths.
//!
//! Table parameters mirror Linux dm-flakey:
//!
//! ```text
//! <dev> <start> <up_interval> <down_interval> [<num_features> [<feature> ...]]
//! ```
//!
//! Intervals are in seconds. Supported optional features:
//!
//! - `drop_writes`: during down intervals, writes are silently dropped
//!   (reported as successful without reaching the device) while reads are
//!   serviced normally;
//! - `error_writes`: during down intervals, writes fail with an I/O error
//!   while reads are serviced normally.
//!
//! With no feature, every bio fails during down intervals.
//!
//! Reference: Linux `Documentation/admin-guide/device-mapper/flakey.rst`.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::fmt;

use aster_block::{
    BlockDevice, BlockDeviceLease, BlockDeviceMeta,
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
};
use device_id::DeviceId;
use ostd::timer::{Jiffies, TIMER_FREQ};

use crate::target::{DmTargetMetadata, Target, TargetStatusMode};

/// A `flakey` target.
///
/// The up/down phase is computed lazily from the elapsed time since target
/// creation, exactly like Linux's `flakey_map`: no background timer is
/// needed. See [`FlakeyTarget::is_up`].
pub struct FlakeyTarget {
    /// The underlying block device (used for I/O).
    device: Arc<dyn BlockDevice>,
    /// The device ID of the underlying device.
    device_id: DeviceId,
    /// The starting physical sector on the underlying device.
    physical_start: u64,
    /// Length of each up interval, in seconds.
    up_interval: u64,
    /// Length of each down interval, in seconds.
    down_interval: u64,
    /// Whether writes are silently dropped during down intervals.
    drop_writes: bool,
    /// Whether writes fail with an I/O error during down intervals while
    /// reads are still serviced.
    error_writes: bool,
    /// Jiffies at target creation; the up phase starts here.
    start_jiffies: u64,
    /// Pre-formatted parameters string for `DM_TABLE_STATUS`.
    params: String,
    /// Lease preventing the underlying device from being unregistered.
    /// `None` only when the device is not in the block registry (tests).
    _lease: Option<BlockDeviceLease>,
}

/// Metadata of the `flakey` target type.
//
// Matches Linux v6.12 LTS dm-flakey version. The optional features
// implemented here (`drop_writes`, `error_writes`) are exactly the ones
// covered by upstream 1.2.0.
pub const METADATA: DmTargetMetadata = DmTargetMetadata::new("flakey", [1, 2, 0]);

impl FlakeyTarget {
    /// Creates a new `flakey` target.
    ///
    /// `physical_start` is the offset (in sectors) on the underlying device
    /// where this target's region begins. `up_interval` and `down_interval`
    /// are in seconds; their sum must be non-zero. `drop_writes` and
    /// `error_writes` select the down-interval behavior and are mutually
    /// exclusive (enforced by the parser).
    ///
    /// If the device is registered in the block registry, a lease is acquired
    /// automatically. This prevents the underlying device from being
    /// unregistered while this target exists.
    pub fn new(
        device: Arc<dyn BlockDevice>,
        physical_start: u64,
        up_interval: u64,
        down_interval: u64,
        drop_writes: bool,
        error_writes: bool,
    ) -> Self {
        let lease = aster_block::lookup_lease(device.id());
        let device = lease.as_ref().map(|l| l.device().clone()).unwrap_or(device);
        let device_id = device.id();
        let base = format!(
            "{}:{} {} {} {}",
            device_id.major().get(),
            device_id.minor().get(),
            physical_start,
            up_interval,
            down_interval
        );
        let params = if drop_writes {
            format!("{} 1 drop_writes", base)
        } else if error_writes {
            format!("{} 1 error_writes", base)
        } else {
            format!("{} 0", base)
        };
        Self {
            device,
            device_id,
            physical_start,
            up_interval,
            down_interval,
            drop_writes,
            error_writes,
            start_jiffies: Jiffies::elapsed().as_u64(),
            params,
            _lease: lease,
        }
    }

    /// Returns whether the target is currently in an up interval.
    ///
    /// Mirrors Linux's `flakey_map`: the phase alternates with a period of
    /// `up_interval + down_interval` seconds starting at target creation,
    /// with the up phase first. `up_interval == 0` keeps the device always
    /// down; `down_interval == 0` keeps it always up.
    fn is_up(&self) -> bool {
        let period = self.up_interval + self.down_interval;
        debug_assert!(period > 0, "flakey: up + down interval must be non-zero");
        let elapsed_secs = Jiffies::elapsed()
            .as_u64()
            .saturating_sub(self.start_jiffies)
            / TIMER_FREQ;
        elapsed_secs % period < self.up_interval
    }

    /// Forwards the bio to the underlying device with the sector offset
    /// adjusted, identical to the linear target's remapping.
    fn map_pass(&self, mut bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError> {
        // sid_offset = physical_start - logical_start (may be negative);
        // accumulate with any existing offset for stacked DM devices.
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
}

impl Target for FlakeyTarget {
    fn name(&self) -> &str {
        "flakey"
    }

    fn map_bio(&self, bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError> {
        if self.is_up() {
            return self.map_pass(bio, logical_start);
        }
        // Down interval. With drop_writes/error_writes only writes are
        // affected and reads are serviced normally; otherwise every bio
        // fails.
        match bio.type_() {
            BioType::Write => {
                if self.drop_writes {
                    // Silently dropped: report success without touching the
                    // underlying device.
                    bio.complete(BioStatus::Complete);
                } else {
                    bio.complete(BioStatus::IoError);
                }
                Ok(())
            }
            BioType::Read | BioType::Flush => {
                if self.drop_writes || self.error_writes {
                    self.map_pass(bio, logical_start)
                } else {
                    bio.complete(BioStatus::IoError);
                    Ok(())
                }
            }
        }
    }

    fn metadata(&self) -> BlockDeviceMeta {
        self.device.metadata()
    }

    fn status_params(&self, mode: TargetStatusMode) -> String {
        match mode {
            TargetStatusMode::Table => self.params.clone(),
            // Linux dm-flakey STATUSTYPE_INFO reports the current phase.
            TargetStatusMode::Status => String::from(if self.is_up() { "up" } else { "down" }),
        }
    }

    fn deps(&self) -> &[DeviceId] {
        core::slice::from_ref(&self.device_id)
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        Vec::from([self.device.clone()])
    }
}

impl fmt::Debug for FlakeyTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FlakeyTarget")
            .field("device_id", &self.device_id)
            .field("physical_start", &self.physical_start)
            .field("up_interval", &self.up_interval)
            .field("down_interval", &self.down_interval)
            .field("drop_writes", &self.drop_writes)
            .field("error_writes", &self.error_writes)
            .finish()
    }
}
