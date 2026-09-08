// SPDX-License-Identifier: MPL-2.0

//! The `striped` target.
//!
//! A [`StripedTarget`] distributes contiguous logical sectors across multiple
//! underlying block devices in a round-robin fashion, using fixed-size stripes.
//! This is the device-mapper equivalent of RAID0 striping and is used by LVM
//! striped logical volumes (e.g., `lvcreate -i 2 -I 64`).
//!
//! Each stripe maps a `stripe_size`-sector-wide region to a single underlying
//! device at a given start offset. The first `stripe_size` sectors of the
//! mapped device go to stripe 0, the next `stripe_size` sectors to stripe 1,
//! and so on, wrapping around after `num_stripes` stripes.
//!
//! Reference: Linux `Documentation/admin-guide/device-mapper/striped.rst` and
//! `drivers/md/dm-stripe.c`.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::{fmt, ops::Range};

use aster_block::{
    BlockDevice, BlockDeviceLease, BlockDeviceMeta,
    bio::{BioEnqueueError, BioStatus, SubmittedBio},
    id::Sid,
};
use device_id::DeviceId;
use smallvec::SmallVec;

use crate::target::Target;

/// A single stripe of a [`StripedTarget`].
struct Stripe {
    /// The underlying block device for this stripe.
    device: Arc<dyn BlockDevice>,
    /// The starting physical sector on the underlying device.
    start: u64,
    /// Lease preventing the underlying device from being unregistered.
    _lease: Option<BlockDeviceLease>,
}

/// A striped mapping target (RAID0-style striping).
///
/// Maps logical sector `L` on the mapped device to physical sector
/// `stripes[i].start + (L - logical_start) % stripe_size` on
/// `stripes[i].device`, where
/// `i = ((L - logical_start) / stripe_size) % num_stripes`.
///
/// With a single stripe, this target is semantically identical to
/// [`LinearTarget`](crate::targets::linear::LinearTarget); LVM2 uses the
/// `striped` target type for all logical volumes, including single-stripe ones.
pub struct StripedTarget {
    stripes: Vec<Stripe>,
    /// Stripe size (chunk size) in sectors.
    stripe_size: u64,
    /// Cached metadata computed from the stripe devices.
    metadata: BlockDeviceMeta,
    /// Pre-formatted parameters string for `DM_TABLE_STATUS`.
    params: String,
    /// Pre-collected device IDs for `DM_TABLE_DEPS`.
    deps: Vec<DeviceId>,
}

/// Registers the `striped` target type and its version.
pub fn register() {
    // Linux dm-stripe 1.6.0; basic multi-stripe (RAID0) is implemented.
    crate::register_target_type("striped", [1, 6, 0]);
}

impl StripedTarget {
    /// Creates a new striped target.
    ///
    /// `stripes` is a list of `(device, start_sector)` pairs, one per stripe.
    /// `stripe_size` is the width of each stripe in sectors and must be a
    /// non-zero power of two (Linux `dm-stripe` requirement). Callers that
    /// accept untrusted input must validate it before calling this constructor
    /// (the shared target parser does so); direct callers passing an invalid
    /// `stripe_size` will panic.
    pub fn new(stripes: Vec<(Arc<dyn BlockDevice>, u64)>, stripe_size: u64) -> Self {
        assert!(
            !stripes.is_empty(),
            "striped target requires at least one stripe"
        );
        assert!(stripe_size > 0, "stripe size must be non-zero");
        assert!(
            stripe_size.is_power_of_two(),
            "stripe size must be a power of two"
        );

        let stripes: Vec<Stripe> = stripes
            .into_iter()
            .map(|(device, start)| {
                let lease = aster_block::lookup_lease(device.id());
                let device = lease.as_ref().map(|l| l.device().clone()).unwrap_or(device);
                Stripe {
                    device,
                    start,
                    _lease: lease,
                }
            })
            .collect();

        let max_nr_segments_per_bio = stripes
            .iter()
            .map(|s| s.device.metadata().max_nr_segments_per_bio)
            .min()
            .unwrap_or(usize::MAX);

        // Format: "<num_stripes> <stripe_size> <dev1> <start1> [<dev2> <start2> ...]"
        let mut params = format!("{} {}", stripes.len(), stripe_size);
        let mut deps = Vec::with_capacity(stripes.len());
        for stripe in &stripes {
            let id = stripe.device.id();
            params.push_str(&format!(" {}:{}", id.major().get(), id.minor().get()));
            params.push_str(&format!(" {}", stripe.start));
            deps.push(id);
        }

        Self {
            stripes,
            stripe_size,
            metadata: BlockDeviceMeta {
                max_nr_segments_per_bio,
                nr_sectors: 0,
            },
            params,
            deps,
        }
    }

    /// Computes the `sid_offset` delta for absolute stripe `abs_stripe`.
    ///
    /// RAID0 striping wraps around: absolute stripe `a` is served by physical
    /// device `a % num_stripes`, and the `a / num_stripes`-th round of that
    /// device is used. For a bio entirely within absolute stripe `a`:
    ///   physical_sector = stripes[a % num_stripes].start
    ///                   + (a / num_stripes) * stripe_size + offset_in_stripe
    ///   current_sector  = logical_start + a * stripe_size + offset_in_stripe
    /// so the delta (added to `sid_offset`) is
    ///   stripes[a % num_stripes].start + (a / num_stripes) * stripe_size
    ///   - logical_start - a * stripe_size.
    fn stripe_delta(&self, abs_stripe: u64, logical_start: u64) -> Result<i64, BioEnqueueError> {
        let num_stripes = self.stripes.len() as u64;
        let phys = (abs_stripe % num_stripes) as usize;
        let round = abs_stripe / num_stripes;
        let s = &self.stripes[phys];
        let physical_sector = (s.start as u128)
            .checked_add(
                (round as u128)
                    .checked_mul(self.stripe_size as u128)
                    .ok_or(BioEnqueueError::Refused)?,
            )
            .ok_or(BioEnqueueError::Refused)?;
        let logical_sector = (logical_start as u128)
            .checked_add(
                (abs_stripe as u128)
                    .checked_mul(self.stripe_size as u128)
                    .ok_or(BioEnqueueError::Refused)?,
            )
            .ok_or(BioEnqueueError::Refused)?;
        let delta = physical_sector as i128 - logical_sector as i128;
        i64::try_from(delta).map_err(|_| BioEnqueueError::Refused)
    }

    /// Returns the physical device index (0..num_stripes) for absolute stripe
    /// `abs_stripe`, accounting for RAID0 wrap-around.
    fn phys_index(&self, abs_stripe: u64) -> usize {
        (abs_stripe % self.stripes.len() as u64) as usize
    }
}

impl Target for StripedTarget {
    fn name(&self) -> &str {
        "striped"
    }

    fn map_bio(&self, mut bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError> {
        // Current-layer sector range (after applying the accumulated offset).
        let cur = bio.current_sid_range();
        let cur_start = cur.start.to_raw();
        let cur_end = cur.end.to_raw();

        // Zero-length bios (e.g., flush) have no sector range to map. Forward
        // the flush to the first stripe's device, matching the convention that
        // flush is only forwarded to the first target.
        if cur_end <= cur_start {
            let delta = self.stripe_delta(0, logical_start)?;
            let new_offset = bio
                .sid_offset()
                .checked_add(delta)
                .ok_or(BioEnqueueError::Refused)?;
            bio.set_sid_offset(new_offset);
            return self.stripes[0].device.enqueue(bio);
        }

        // Relative sectors within this striped target's region.
        let rel_start = cur_start - logical_start;
        let rel_end = cur_end - logical_start;

        // Absolute stripe indices (before the modulo wrap-around). Each absolute
        // stripe may map to a different physical device, so a bio spanning
        // several absolute stripes must be split at every stripe boundary.
        let first_abs = rel_start / self.stripe_size;
        let last_abs = (rel_end - 1) / self.stripe_size;

        if first_abs == last_abs {
            // The bio lies within a single absolute stripe - fast path.
            let delta = self.stripe_delta(first_abs, logical_start)?;
            let new_offset = bio
                .sid_offset()
                .checked_add(delta)
                .ok_or(BioEnqueueError::Refused)?;
            bio.set_sid_offset(new_offset);
            return self.stripes[self.phys_index(first_abs)].device.enqueue(bio);
        }

        // The bio spans multiple absolute stripes - split at every stripe
        // boundary and forward each child to its (possibly different) device.
        let mut ranges = SmallVec::<[Range<Sid>; 8]>::new();
        let mut cursor = cur_start;
        let mut a = first_abs;
        while a <= last_abs {
            let boundary = logical_start + (a + 1) * self.stripe_size;
            let part_end = core::cmp::min(boundary, cur_end);
            ranges.push(Sid::new(cursor)..Sid::new(part_end));
            cursor = part_end;
            a += 1;
        }

        let (children, completion) = bio.split(&ranges)?;
        // Zip children with the absolute stripe range directly to avoid building
        // a second temporary Vec.
        for (mut child, abs) in children.into_iter().zip(first_abs..=last_abs) {
            let delta = self.stripe_delta(abs, logical_start)?;
            let new_offset = child
                .sid_offset()
                .checked_add(delta)
                .ok_or(BioEnqueueError::Refused)?;
            child.set_sid_offset(new_offset);
            if self.stripes[self.phys_index(abs)]
                .device
                .enqueue(child)
                .is_err()
            {
                completion.complete_child(BioStatus::IoError);
            }
        }
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        // The mapped device's segment limit is the most restrictive among the
        // underlying stripe devices so that a bio satisfying the mapped
        // device's limit satisfies every stripe's device. The value was
        // computed at construction time and is cached here.
        self.metadata
    }

    fn params(&self) -> &str {
        &self.params
    }

    fn deps(&self) -> &[DeviceId] {
        &self.deps
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        self.stripes.iter().map(|s| s.device.clone()).collect()
    }
}

impl fmt::Debug for StripedTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("StripedTarget")
            .field("num_stripes", &self.stripes.len())
            .field("stripe_size", &self.stripe_size)
            .finish()
    }
}
