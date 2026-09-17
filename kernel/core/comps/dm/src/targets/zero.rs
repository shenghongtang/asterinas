// SPDX-License-Identifier: MPL-2.0

//! The `zero` target.
//!
//! Reads return zero-filled blocks and writes are silently discarded. It is
//! useful as a placeholder backing store and for tests.
//!
//! Reference: Linux `Documentation/admin-guide/device-mapper/zero.rst`.

use alloc::{string::String, sync::Arc, vec::Vec};

use aster_block::{
    BlockDevice, BlockDeviceMeta,
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
};

use crate::target::{DmTargetMetadata, Target, TargetStatusMode};

/// A `zero` target.
#[derive(Debug)]
pub struct ZeroTarget {
    num_sectors: u64,
}

/// Metadata of the `zero` target type.
//
// Linux dm-zero 1.1.0 (discard support); discard is handled by the block
// layer which reports unsupported when the target does not implement it.
pub const METADATA: DmTargetMetadata = DmTargetMetadata::new("zero", [1, 1, 0]);

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
        // This match is exhaustive on purpose: every bio that reaches the
        // target must be completed here. When new mutating bio types
        // (discard, write-zeroes) are added to `BioType`, the compiler will
        // force this match to be updated; they must complete successfully
        // (`Complete`) just like writes, since the zero target has no backing
        // store and any mutation trivially succeeds.
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
            // Saturate on 32-bit platforms instead of silently truncating.
            nr_sectors: usize::try_from(self.num_sectors).unwrap_or(usize::MAX),
        }
    }

    fn status_params(&self, _mode: TargetStatusMode) -> String {
        // Linux dm-zero reports neither table parameters nor runtime status.
        String::new()
    }

    fn deps(&self) -> &[device_id::DeviceId] {
        &[]
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        Vec::new()
    }
}
