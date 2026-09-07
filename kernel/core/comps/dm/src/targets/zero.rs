// SPDX-License-Identifier: MPL-2.0

//! The `zero` target.
//!
//! Reads return zero-filled blocks and writes are silently discarded. It is
//! useful as a placeholder backing store and for tests.
//!
//! Reference: Linux `Documentation/admin-guide/device-mapper/zero.rst`.

use alloc::{sync::Arc, vec::Vec};

use aster_block::{
    BlockDevice, BlockDeviceMeta,
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
};

use crate::target::Target;

/// A `zero` target.
#[derive(Debug)]
pub struct ZeroTarget {
    num_sectors: u64,
}

/// Registers the `zero` target type and its version.
pub fn register() {
    crate::register_target_type("zero", [1, 0, 0]);
}

impl ZeroTarget {
    /// Creates a new `zero` target.
    pub fn new(num_sectors: u64) -> Self {
        Self { num_sectors }
    }
}

impl Target for ZeroTarget {
    fn name(&self) -> &str {
        "zero"
    }

    fn map_bio(&self, bio: SubmittedBio, _logical_start: u64) -> Result<(), BioEnqueueError> {
        match bio.type_() {
            // Reads: complete with `BioStatus::Zeros` so the upper layer (page
            // cache or block device read path) fills the buffer with zeros.
            // This avoids writing to the DMA buffer directly from the target,
            // which may fail due to DMA direction restrictions.
            BioType::Read => {
                bio.complete(BioStatus::Zeros);
            }
            BioType::Write | BioType::Flush => {
                bio.complete(BioStatus::Complete);
            }
        }
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.num_sectors as usize,
        }
    }

    fn params(&self) -> &str {
        ""
    }

    fn deps(&self) -> &[device_id::DeviceId] {
        &[]
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        Vec::new()
    }
}
