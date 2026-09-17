// SPDX-License-Identifier: MPL-2.0

//! The `error` target.
//!
//! Every read and write fails with an I/O error. It is used to fence off
//! regions that must never be accessed and to exercise error paths.
//!
//! Reference: Linux `Documentation/admin-guide/device-mapper/zero.rst`
//! (the `error` target is documented alongside `zero`).

use alloc::{string::String, sync::Arc, vec::Vec};

use aster_block::{
    BlockDevice, BlockDeviceMeta,
    bio::{BioEnqueueError, BioStatus, SubmittedBio},
};
use device_id::DeviceId;

use crate::target::{DmTargetMetadata, Target, TargetStatusMode};

/// An `error` target.
#[derive(Debug)]
pub struct ErrorTarget {
    num_sectors: u64,
}

/// Metadata of the `error` target type.
//
// Matches Linux's dm-error version (internal changes only).
pub const METADATA: DmTargetMetadata = DmTargetMetadata::new("error", [1, 5, 0]);

impl ErrorTarget {
    /// Creates a new `error` target.
    pub fn new(num_sectors: u64) -> Self {
        Self { num_sectors }
    }
}

impl Target for ErrorTarget {
    fn name(&self) -> &str {
        "error"
    }

    fn map_bio(&self, bio: SubmittedBio, _logical_start: u64) -> Result<(), BioEnqueueError> {
        bio.complete(BioStatus::IoError);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            // Saturate on 32-bit platforms instead of silently truncating.
            nr_sectors: usize::try_from(self.num_sectors).unwrap_or(usize::MAX),
        }
    }

    fn status_params(&self, _mode: TargetStatusMode) -> String {
        // Linux dm-error reports neither table parameters nor runtime status.
        String::new()
    }

    fn deps(&self) -> &[DeviceId] {
        &[]
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        Vec::new()
    }
}
