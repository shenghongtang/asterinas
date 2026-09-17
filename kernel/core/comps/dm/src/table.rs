// SPDX-License-Identifier: MPL-2.0

//! The device mapper table.
//!
//! A [`DmTable`] maps logical sector ranges of a mapped device to underlying
//! targets. The table is composed of contiguous, non-overlapping entries,
//! each specifying a target that handles I/O for that region.
//!
//! Tables are built by calling [`DmTable::add_target`] with increasing
//! `logical_start` values. The table validates that entries are contiguous
//! and non-overlapping.

use alloc::{boxed::Box, collections::BTreeSet, string::String, sync::Arc, vec::Vec};
use core::{
    ops::Range,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use aster_block::{
    BlockDevice, BlockDeviceMeta,
    bio::{Bio, BioCompleteFn, BioEnqueueError, BioStatus, BioType, SubmittedBio},
    id::Sid,
};
use device_id::DeviceId;
use io_util::batch::IoBatch;
use ostd::sync::{LocalIrqDisabled, SpinLock};
use smallvec::SmallVec;

use crate::{
    TableError,
    target::{DmTarget, Target, TargetStatusMode},
};

/// The maximum number of table-entry-aligned sub-ranges a single bio is
/// expected to span, stored inline by [`BioParts`] before spilling to the heap.
const BIO_PARTS_INLINE_CAP: usize = 4;

/// The result of splitting a bio's sector range into table-entry-aligned parts.
///
/// Each element is a sub-range (in the table's logical sector coordinates) and
/// a reference to the table entry that handles I/O for that sub-range.
type BioParts<'a> = SmallVec<[(Range<Sid>, &'a TableEntry); BIO_PARTS_INLINE_CAP]>;

/// A device mapper table mapping logical sectors to targets.
///
/// The table is immutable after construction. To modify the mapping of a
/// mapped device, create a new table and load it via
/// [`MappedDevice::load_table`] (used by `DM_TABLE_LOAD` + resume).
pub struct DmTable {
    /// Sorted, contiguous entries covering the entire mapped device.
    entries: Vec<TableEntry>,
    /// Total number of sectors spanned by all entries.
    total_sectors: u64,
    /// Cached metadata computed from the targets in the table.
    metadata: BlockDeviceMeta,
    /// Cached unique underlying devices, computed during construction.
    cached_devices: Vec<Arc<dyn BlockDevice>>,
    /// Device IDs already collected in `cached_devices` (avoids duplicates).
    seen_device_ids: BTreeSet<DeviceId>,
}

/// A single entry in the device mapper table.
struct TableEntry {
    /// The starting logical sector on the mapped device.
    logical_start: u64,
    /// The number of sectors covered by this entry.
    num_sectors: u64,
    /// The target that handles I/O for this region.
    target: DmTarget,
}

/// Information about a target in the table, used for `DM_TABLE_STATUS` output.
#[derive(Debug, Clone)]
pub struct TargetInfo {
    /// The starting logical sector on the mapped device.
    pub sector_start: u64,
    /// The number of sectors covered by this target.
    pub length: u64,
    /// The target type name (e.g., `"linear"`).
    pub target_type: String,
    /// The target-specific parameters or runtime status string, depending on
    /// the [`TargetStatusMode`] passed to [`DmTable::target_infos`].
    pub params: String,
}

impl DmTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            total_sectors: 0,
            metadata: BlockDeviceMeta {
                max_nr_segments_per_bio: usize::MAX,
                nr_sectors: 0,
            },
            cached_devices: Vec::new(),
            seen_device_ids: BTreeSet::new(),
        }
    }

    /// Adds a target to the table.
    ///
    /// The `logical_start` must be equal to the end of the previous entry
    /// (i.e., `logical_start == 0` for the first entry, or
    /// `logical_start == previous_logical_start + previous_num_sectors`).
    /// This ensures entries are contiguous and non-overlapping.
    ///
    /// # Errors
    ///
    /// Returns [`TableError::ZeroLength`] if `num_sectors` is zero, or
    /// [`TableError::NotContiguous`] if `logical_start` does not match the
    /// expected contiguous position.
    pub fn add_target(
        &mut self,
        logical_start: u64,
        num_sectors: u64,
        target: DmTarget,
    ) -> Result<(), TableError> {
        if num_sectors == 0 {
            return Err(TableError::ZeroLength);
        }
        if logical_start != self.total_sectors {
            return Err(TableError::NotContiguous);
        }

        self.total_sectors = self
            .total_sectors
            .checked_add(num_sectors)
            .ok_or(TableError::TooLarge)?;
        let target_metadata = target.metadata();
        self.metadata.max_nr_segments_per_bio = self
            .metadata
            .max_nr_segments_per_bio
            .min(target_metadata.max_nr_segments_per_bio);
        self.metadata.nr_sectors =
            usize::try_from(self.total_sectors).map_err(|_| TableError::TooLarge)?;

        // Collect unique underlying devices now so map_flush does not have to
        // recompute this set on every flush request.
        for dev in target.underlying_devices() {
            if self.seen_device_ids.insert(dev.id()) {
                self.cached_devices.push(dev);
            }
        }

        self.entries.push(TableEntry {
            logical_start,
            num_sectors,
            target,
        });
        Ok(())
    }

    /// Returns the total number of sectors spanned by the table.
    pub fn total_sectors(&self) -> u64 {
        self.total_sectors
    }

    /// Returns the number of targets in the table.
    pub fn num_targets(&self) -> usize {
        self.entries.len()
    }

    /// Returns the target whose region contains `sector`, if any.
    ///
    /// Used by `DM_TARGET_MSG` to route a message to the correct target.
    /// `sector` is compared against each entry's
    /// `[logical_start, logical_start + num_sectors)` range.
    ///
    /// Uses binary search via [`Vec::partition_point`] since entries are
    /// kept sorted by `logical_start`, giving O(log n) lookup.
    pub fn find_target(&self, sector: u64) -> Option<&DmTarget> {
        // `partition_point` returns the first index where the predicate is
        // false. Entries before that index have `logical_start <= sector`, so
        // the entry at `idx - 1` is the candidate if it covers `sector`.
        let idx = self.entries.partition_point(|e| e.logical_start <= sector);
        let entry = idx.checked_sub(1).and_then(|i| self.entries.get(i))?;
        let entry_end = entry.logical_start + entry.num_sectors;
        (sector < entry_end).then_some(&entry.target)
    }

    /// Maps a submitted bio to the appropriate target and forwards it.
    ///
    /// Flush bios (which have an empty sector range) are fanned out to all
    /// underlying devices referenced by the table, so every target's backing
    /// device receives the flush.
    ///
    /// If the bio spans multiple targets in the table, it is automatically
    /// split at the target boundaries. Each child bio is forwarded to its
    /// corresponding target, and the original bio is completed once all
    /// children have finished.
    ///
    /// # Errors
    ///
    /// Returns [`BioEnqueueError::Refused`] if the bio's start sector is
    /// outside the table's range.
    pub fn map_bio(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        // Flush bios have an empty sector range; forward to the first target.
        if bio.type_() == BioType::Flush {
            return self.map_flush(bio);
        }

        // Apply the accumulated sid_offset for the table lookup. When DM
        // devices are nested (e.g., dmvg-biglv → testlv2 → /dev/vdf), the
        // outer device's LinearTarget adjusts sid_offset but leaves sid_range
        // unchanged. Without this adjustment, the table lookup would use the
        // outer device's logical sector, which is outside the inner device's
        // table range, causing a spurious "Refused" (EBUSY) error.
        let offset = bio.sid_offset();
        let bio_range = bio.sid_range().clone();
        let adjusted_start = bio_range
            .start
            .to_raw()
            .checked_add_signed(offset)
            .ok_or(BioEnqueueError::Refused)?;
        let adjusted_end = bio_range
            .end
            .to_raw()
            .checked_add_signed(offset)
            .ok_or(BioEnqueueError::Refused)?;

        // Split the adjusted range at target boundaries.
        let parts = self.bio_parts(adjusted_start, adjusted_end)?;

        if parts.is_empty() {
            // A zero-length non-flush bio has no sector range to map.
            // Complete it immediately as a no-op instead of panicking on
            // the `parts.len() == 1` fast path below.
            bio.complete(BioStatus::Complete);
            return Ok(());
        }

        if parts.len() == 1 {
            // The bio fits within a single target - forward directly.
            let (_range, entry) = parts.into_iter().next().unwrap();
            return entry.target.map_bio(bio, entry.logical_start);
        }

        // The bio spans multiple targets - split it at the boundaries.
        let ranges: SmallVec<[Range<Sid>; BIO_PARTS_INLINE_CAP]> =
            parts.iter().map(|(range, _)| range.clone()).collect();
        let (children, completion) = bio.split(&ranges)?;

        for (child, (_range, entry)) in children.into_iter().zip(parts) {
            if entry.target.map_bio(child, entry.logical_start).is_err() {
                // If a child cannot be enqueued, report the failure so the
                // original bio is still completed with an error status.
                completion.complete_child(BioStatus::IoError);
            }
        }
        Ok(())
    }

    /// Splits the sector range `[start, end)` into sub-ranges aligned with
    /// table entry boundaries.
    ///
    /// Each returned tuple contains the sub-range (in the current layer's
    /// coordinate system) and a reference to the table entry that handles it.
    fn bio_parts(&self, start: u64, end: u64) -> Result<BioParts<'_>, BioEnqueueError> {
        let mut cursor = start;
        let mut parts = SmallVec::new();
        while cursor < end {
            let idx = self.entries.partition_point(|e| e.logical_start <= cursor);
            let entry = idx
                .checked_sub(1)
                .and_then(|i| self.entries.get(i))
                .ok_or(BioEnqueueError::Refused)?;

            let entry_end = entry.logical_start + entry.num_sectors;
            if cursor >= entry_end {
                return Err(BioEnqueueError::Refused);
            }

            let part_end = core::cmp::min(end, entry_end);
            parts.push((Sid::new(cursor)..Sid::new(part_end), entry));
            cursor = part_end;
        }
        Ok(parts)
    }

    /// Returns the unique underlying block devices that should receive flush
    /// requests for this table.
    ///
    /// Devices are de-duplicated by device ID so that a device referenced by
    /// multiple targets is flushed only once.
    pub fn underlying_devices(&self) -> &[Arc<dyn BlockDevice>] {
        &self.cached_devices
    }

    /// Forwards a flush bio to every unique underlying device.
    ///
    /// Device mapper flushes must reach all devices that back the mapped
    /// device. For a single underlying device the original bio is forwarded
    /// directly; for multiple devices child flush bios are created and their
    /// completion is coordinated so that the original bio finishes only after
    /// every child has reported.
    fn map_flush(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        let devices = self.underlying_devices();
        match devices.len() {
            0 => {
                bio.complete(BioStatus::Complete);
                Ok(())
            }
            1 => devices[0].enqueue(bio),
            _ => {
                let completion = Arc::new(MultiFlushCompletion::new(bio, devices.len()));
                for device in devices {
                    let child_completion = completion.clone();
                    let complete_fn: BioCompleteFn = Box::new(move |status| {
                        child_completion.complete_child(status);
                    });
                    let child =
                        Bio::new(BioType::Flush, Sid::new(0), Vec::new(), Some(complete_fn));
                    let mut io_batch = IoBatch::new();
                    if child.submit(device.as_ref(), &mut io_batch).is_err() {
                        completion.complete_child(BioStatus::IoError);
                    }
                }
                Ok(())
            }
        }
    }

    /// Computes the metadata for a mapped device backed by this table.
    ///
    /// The `nr_sectors` is the total across all entries. The
    /// `max_nr_segments_per_bio` is the minimum across all targets so that
    /// a bio satisfying the mapped device's limit will also satisfy every
    /// underlying device's limit.
    pub fn metadata(&self) -> BlockDeviceMeta {
        self.metadata
    }

    /// Returns an iterator over the logical sector ranges of the entries.
    ///
    /// This is useful for diagnostics and testing.
    pub fn entry_ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.entries
            .iter()
            .map(|e| e.logical_start..e.logical_start + e.num_sectors)
    }

    /// Returns information about all targets in the table, for `DM_TABLE_STATUS`.
    ///
    /// `mode` selects whether each entry's `params` field holds the table
    /// parameters as loaded (`dmsetup table`) or the runtime status string
    /// (`dmsetup status`); see [`TargetStatusMode`].
    pub fn target_infos(&self, mode: TargetStatusMode) -> Vec<TargetInfo> {
        self.entries
            .iter()
            .map(|e| TargetInfo {
                sector_start: e.logical_start,
                length: e.num_sectors,
                target_type: e.target.name().into(),
                params: e.target.status_params(mode),
            })
            .collect()
    }

    /// Returns the unique list of underlying device IDs that this table
    /// depends on, for `DM_TABLE_DEPS`.
    ///
    /// Iterates over all targets, collects each target's dependencies, and
    /// de-duplicates them so that a device referenced by multiple targets
    /// appears only once.
    pub fn deps(&self) -> Vec<DeviceId> {
        let mut ids: BTreeSet<DeviceId> = BTreeSet::new();
        for entry in &self.entries {
            for dev in entry.target.deps() {
                ids.insert(*dev);
            }
        }
        ids.into_iter().collect()
    }
}

/// Coordinates completion of a flush bio forwarded to multiple underlying
/// block devices.
struct MultiFlushCompletion {
    /// Number of child flushes that have not yet completed.
    remaining: AtomicUsize,
    /// Aggregate status; the first non-`Complete` status wins.
    status: AtomicU32,
    /// The original flush bio, completed once all children have reported.
    original: SpinLock<Option<SubmittedBio>, LocalIrqDisabled>,
}

impl MultiFlushCompletion {
    fn new(original: SubmittedBio, count: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(count),
            status: AtomicU32::new(BioStatus::Complete as u32),
            original: SpinLock::new(Some(original)),
        }
    }

    fn complete_child(&self, status: BioStatus) {
        if status != BioStatus::Complete {
            let _ = self.status.compare_exchange(
                BioStatus::Complete as u32,
                status as u32,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }

        let previous = self.remaining.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        if previous == 1 {
            let status = BioStatus::try_from(self.status.load(Ordering::Acquire)).unwrap();
            let original = self
                .original
                .lock()
                .take()
                .expect("multi-device flush original bio must complete exactly once");
            original.complete(status);
        }
    }
}

impl Default for DmTable {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for DmTable {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("DmTable")
            .field("num_targets", &self.entries.len())
            .field("total_sectors", &self.total_sectors)
            .finish()
    }
}
