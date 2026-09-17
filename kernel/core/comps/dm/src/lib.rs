// SPDX-License-Identifier: MPL-2.0

//! The device mapper (dm) subsystem for Asterinas.
//!
//! The device mapper provides a framework for creating virtual block devices
//! that remap I/O requests to one or more underlying block devices according
//! to a target table. It is the foundation for features such as logical
//! volume management (LVM), disk concatenation, and (in the future) striped
//! volumes, snapshots, and encryption.
//!
//! # Architecture
//!
//! A [`MappedDevice`] implements [`aster_block::BlockDevice`] and appears to
//! the rest of the kernel as an ordinary block device. Internally, it holds a
//! [`DmTable`] that maps logical sector ranges to [`Target`]s. When a bio is
//! enqueued, the table finds the appropriate target and forwards the bio
//! after adjusting the sector offset.
//!
//! # Userspace Control Interface
//!
//! Mapped devices are created and configured from userspace through the
//! `/dev/mapper/control` character device, which accepts Linux-compatible
//! `DM_*` ioctl commands. The typical sequence is:
//!
//! 1. `DM_DEV_CREATE` - create a new mapped device by name
//! 2. `DM_TABLE_LOAD` - load a target table (e.g. linear mappings)
//! 3. `DM_DEV_SUSPEND` - resume the device (clear the suspend flag)
//!
//! See the kernel-side `device::dm::control` module for the ioctl handler.
//!
//! # Current limitations
//!
//! - Only linear, striped, zero, error, and verity targets are implemented.
//!
//! # Usage (kernel API)
//!
//! ```no_run
//! use aster_block::BlockDevice;
//! use aster_dm::{DmTable, MappedDevice};
//! use aster_dm::targets::linear::LinearTarget;
//! use alloc::sync::Arc;
//!
//! use aster_dm::DmTarget;
//!
//! let mut table = DmTable::new();
//! table.add_target(0, 1000, DmTarget::Linear(LinearTarget::new(device_a, 0))).unwrap();
//! table.add_target(1000, 2000, DmTarget::Linear(LinearTarget::new(device_b, 500))).unwrap();
//!
//! let mapped = MappedDevice::create("myvol", table).unwrap();
//! ```

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::{collections::VecDeque, format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aster_block::{
    BlockDevice, BlockDeviceMeta, MajorIdOwner, allocate_major_with_name,
    bio::{BioEnqueueError, BioStatus, SubmittedBio},
};
use component::{ComponentInitError, init_component};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::sync::WaitQueue;
use spin::Once;

use crate::manager::DmManager;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "dm: "
    };
}

pub mod hash;
pub mod manager;
pub mod parser;
mod registry;
pub mod sha256;
pub mod sha512;
pub mod table;
pub mod target;
pub mod targets;

static DM_CREATE_ARGS: Once<Vec<parser::DmCreateArg>> = Once::new();
aster_cmdline::define_repeatable_kv_param!("dm_mod.create", DM_CREATE_ARGS);

static DM_CREATE_MANDATORY_ARGS: Once<Vec<parser::DmCreateArg>> = Once::new();
aster_cmdline::define_repeatable_kv_param!("dm_mod.create_mandatory", DM_CREATE_MANDATORY_ARGS);

#[cfg(ktest)]
mod test;

pub use parser::parse_target;
pub use table::{DmTable, TargetInfo};
pub use target::{DmTarget, Target, TargetStatusMode};
pub use targets::{
    error::ErrorTarget, linear::LinearTarget, verity::VerityTarget, zero::ZeroTarget,
};

/// The major ID owner for device mapper devices.
static DM_MAJOR: Once<Arc<MajorIdOwner>> = Once::new();

/// The global device mapper manager.
///
/// Initialized in [`init`] after the block major number has been allocated.
/// All public registry operations in this crate delegate to this instance.
static DM_MANAGER: Once<Arc<DmManager>> = Once::new();

/// Returns a reference to the global device mapper manager.
///
/// # Panics
/// Panics if called before [`init`] has run.
fn manager() -> &'static Arc<DmManager> {
    DM_MANAGER.get().expect("DM manager is not initialized")
}

/// Hook invoked after a deferred (`DM_DEFERRED_REMOVE`) removal completes.
///
/// A deferred removal is triggered from `BlockDevice::close`, which lives in
/// this crate, while devtmpfs node cleanup lives in the kernel's
/// `device::dm::control` module. The control module registers this hook at
/// init time so that close-triggered removals can clean up `/dev` nodes.
static DEFERRED_REMOVE_HOOK: Once<fn(&str, DeviceId)> = Once::new();

/// Registers the hook invoked after a deferred removal completes.
///
/// Must be called at most once, during kernel init. The hook receives the
/// removed device's name and ID.
pub fn register_deferred_remove_hook(hook: fn(&str, DeviceId)) {
    DEFERRED_REMOVE_HOOK.call_once(|| hook);
}

/// Returns the allocated block major ID for device mapper devices, if initialized.
pub fn dm_major() -> Option<MajorId> {
    DM_MAJOR.get().map(|owner| owner.get())
}

/// Ensures the dm component is initialized (block major allocated and manager
/// instance created).
///
/// This is called from the kernel's dm module to ensure the block major is
/// allocated even if the component system hasn't called `init()`.
pub fn ensure_initialized() {
    DM_MAJOR.call_once(|| {
        Arc::new(
            allocate_major_with_name("device-mapper")
                .expect("failed to allocate a major ID for device mapper"),
        )
    });
    DM_MANAGER.call_once(|| {
        Arc::new(DmManager::new(
            DM_MAJOR
                .get()
                .expect("failed to get the major ID owner for device mapper")
                .clone(),
        ))
    });
}

/// Returns whether the dm component has already been initialized.
///
/// This allows callers that must run after `aster_dm::init()` to assert on
/// the initialization order instead of silently re-initializing.
pub fn is_initialized() -> bool {
    DM_MAJOR.get().is_some() && DM_MANAGER.get().is_some()
}

/// Resolves a block device by name, `major:minor`, or `dev:<encoded-id>`.
///
/// Supports all three forms used by both the boot-time `dm_mod.create=`
/// parser and the ioctl table loader, so device references are interpreted
/// consistently regardless of entry point.
///
/// Returns `DmError::InvalidParameters` for syntactically invalid input and
/// `DmError::NotFound` when the requested device is not registered.
pub fn lookup_block_device(name_or_id: &str) -> Result<Arc<dyn BlockDevice>, DmError> {
    // `dev:<encoded_id>` form.
    if let Some(raw) = name_or_id.strip_prefix("dev:") {
        let id: u64 = raw
            .parse()
            .map_err(|_| DmError::InvalidParameters("invalid dev id"))?;
        let device_id = DeviceId::from_encoded_u64(id)
            .ok_or(DmError::InvalidParameters("invalid encoded dev id"))?;
        return aster_block::lookup(device_id).ok_or(DmError::NotFound);
    }

    // `major:minor` form.
    if let Some(colon) = name_or_id.find(':') {
        let major: u16 = name_or_id[..colon]
            .parse()
            .map_err(|_| DmError::InvalidParameters("invalid major number"))?;
        let minor: u32 = name_or_id[colon + 1..]
            .parse()
            .map_err(|_| DmError::InvalidParameters("invalid minor number"))?;
        let major_id = MajorId::try_from(major)
            .map_err(|_| DmError::InvalidParameters("major out of range"))?;
        let minor_id = MinorId::try_from(minor)
            .map_err(|_| DmError::InvalidParameters("minor out of range"))?;
        let device_id = DeviceId::new(major_id, minor_id);
        return aster_block::lookup(device_id).ok_or(DmError::NotFound);
    }

    // Device name form (with optional `/dev/` prefix).
    let dev_name = name_or_id.rsplit('/').next().unwrap_or(name_or_id);
    aster_block::collect_all()
        .into_iter()
        .find(|d| d.name() == dev_name)
        .ok_or(DmError::NotFound)
}

/// Returns the list of supported target types and their versions, sorted by
/// name.
///
/// The result is built from the static [`targets::SUPPORTED_TARGETS`] slice;
/// there is no runtime registration step.
pub fn list_target_versions() -> Vec<(&'static str, [u32; 3])> {
    targets::SUPPORTED_TARGETS
        .iter()
        .map(|m| (m.name, m.version))
        .collect()
}

/// Returns the version of the named target type, if supported.
pub fn get_target_version(name: &str) -> Option<[u32; 3]> {
    targets::SUPPORTED_TARGETS
        .iter()
        .find(|m| m.name == name)
        .map(|m| m.version)
}

/// Errors that can occur when loading or validating a device mapper table.
#[derive(Debug)]
pub enum TableError {
    /// Two adjacent targets overlap or leave a gap.
    NotContiguous,
    /// The table is empty (no targets).
    Empty,
    /// A target covers zero sectors.
    ZeroLength,
    /// The total sector count exceeds what fits in the platform `usize`.
    TooLarge,
}

/// Errors that can occur when creating or managing device mapper devices.
#[derive(Debug)]
pub enum DmError {
    /// The device mapper subsystem has not been initialized.
    NotInitialized,
    /// Minor number space is exhausted.
    MinorExhausted,
    /// A device with the same name is already registered.
    AlreadyRegistered,
    /// The named device was not found.
    NotFound,
    /// A table has already been loaded for this device.
    TableAlreadyLoaded,
    /// The device is still in use (has outstanding leases or I/O).
    DeviceBusy,
    /// A device with the same UUID is already registered.
    UuidExists,
    /// The requested minor number is already in use.
    MinorBusy,
    /// The table is invalid.
    InvalidTable(TableError),
    /// The target parameters are invalid.
    InvalidParameters(&'static str),
    /// The target type name is not supported by this kernel.
    UnsupportedTarget,
    /// A target's parameters or geometry are invalid.
    Table(&'static str),
    /// A backing device named in a target specification could not be
    /// resolved (unknown name or malformed device id).
    ResolveBacking(&'static str),
}

/// A virtual block device backed by a device mapper table.
///
/// `MappedDevice` implements [`BlockDevice`], so it can be used anywhere a
/// regular block device is accepted - by filesystems, the page cache, or
/// directly by userspace via `/dev`.
///
/// A device is created in the **suspended** state with no table. I/O is
/// refused until a table is loaded (via [`load_table`](Self::load_table))
/// and the device is resumed (via [`set_suspended`](Self::set_suspended)).
pub struct MappedDevice {
    id: DeviceId,
    name: spin::Mutex<Arc<str>>,
    /// Stable display name based on the device ID (e.g. `dm-<minor>`).
    ///
    /// This name remains unchanged after `DM_DEV_RENAME`, so it can be
    /// returned as `&str` from `BlockDevice::name()`.
    display_name: String,
    /// Optional device UUID (e.g. `LVM-<vg_uuid><lv_uuid>`).
    ///
    /// LVM2 identifies its logical volumes by UUID rather than by name when
    /// issuing `DM_DEV_STATUS`/`DM_DEV_SUSPEND`/`DM_DEV_REMOVE` ioctls. The
    /// UUID is set at creation time via `set_uuid` and consulted by
    /// `lookup_by_uuid`.
    uuid: spin::Mutex<Arc<str>>,
    table: spin::Mutex<Option<Arc<DmTable>>>,
    /// Inactive table - loaded by `DM_TABLE_LOAD` and swapped to active
    /// on resume. Implements Linux's table hot-replacement:
    /// suspend → load new table → resume swaps.
    inactive_table: spin::Mutex<Option<Arc<DmTable>>>,
    /// Read-only flag set at device creation or table load time when
    /// `DM_READONLY_FLAG` is supplied by userspace.
    readonly: AtomicBool,
    /// In-flight I/O tracking for suspend/resume draining.
    io: Arc<DmIoState>,
    /// Serializes suspend/resume so that phase transitions, the table swap,
    /// and deferred replay never interleave with another suspend/resume.
    suspend_lock: spin::Mutex<()>,
    /// Monotonically increasing event number for `DM_DEV_WAIT`.
    event_nr: AtomicU32,
    /// Wait queue woken when the event number increases.
    event_wq: WaitQueue,
    /// Open count reported to userspace via `DmIoctl::open_count`.
    ///
    /// The count is maintained by the block-layer `open`/`close` hooks.
    open_count: AtomicU32,
    /// Set by `DM_DEV_REMOVE` with `DM_DEFERRED_REMOVE` while the device is
    /// busy; the last `close()` then completes the removal.
    deferred_remove: AtomicBool,
}

/// Tracks in-flight I/O, the suspend/resume phase, and the queue of bios
/// that arrived while the device was suspended.
///
/// The phase and the in-flight count are packed into a single `AtomicU32` so
/// that [`submit`](Self::submit) can perform the "admit only if running"
/// check together with the in-flight increment as one atomic CAS. This closes
/// the TOCTOU window where a bio could pass a separate phase check but not
/// yet be counted when [`drain`](Self::drain) runs.
///
/// # Phase encoding (bits 30..=31)
///
/// - `00` = **Running**: bios are submitted against the active table.
/// - `01` = **Suspending**: new bios are deferred; a flush suspend is
///   draining already-submitted bios.
/// - `10` = **Suspended**: no new bios are admitted (they are deferred);
///   the device is ready for a table swap.
///
/// Bits `0..=29` hold the in-flight bio count.
///
/// # Linux alignment
///
/// Linux uses `DMF_BLOCK_IO_FOR_SUSPEND` + `DMF_SUSPENDED` and queues bios
/// into `md->deferred` rather than failing them. On resume the deferred
/// bios are replayed against the (possibly new) active table. The active
/// table is kept alive by an `Arc` held in each bio's completion closure,
/// so a noflush resume can swap tables while old bios still complete
/// against the previous table.
struct DmIoState {
    /// Bits 30..=31: phase. Bits 0..=29: in-flight bio count.
    state: AtomicU32,
    drained: WaitQueue,
    /// Bios that arrived while the device was not Running; replayed on
    /// resume.
    deferred: spin::Mutex<VecDeque<SubmittedBio>>,
    /// Set when the device is being destroyed so that newly-arriving bios
    /// complete with an error instead of being deferred forever.
    freeing: AtomicBool,
}

/// Phase bits occupy the top two bits of [`DmIoState::state`].
const PHASE_SHIFT: u32 = 30;
const PHASE_MASK: u32 = 0b11 << PHASE_SHIFT;
const IN_FLIGHT_MASK: u32 = !PHASE_MASK;
const PHASE_RUNNING: u32 = 0;
const PHASE_SUSPENDING: u32 = 1 << PHASE_SHIFT;
const PHASE_SUSPENDED: u32 = 2 << PHASE_SHIFT;

impl DmIoState {
    fn new() -> Self {
        Self {
            // Start in the Suspended phase, matching Linux's behavior where
            // newly created DM devices are suspended until an explicit resume
            // (DM_DEV_SUSPEND without DM_SUSPEND_FLAG). This causes the
            // DM_DEV_CREATE response to include DM_SUSPEND_FLAG, which makes
            // libdevmapper increment its `_suspended_dev_counter`, ensuring
            // the subsequent resume ioctl is actually sent rather than skipped.
            state: AtomicU32::new(PHASE_SUSPENDED),
            drained: WaitQueue::new(),
            deferred: spin::Mutex::new(VecDeque::new()),
            freeing: AtomicBool::new(false),
        }
    }

    /// Atomically admits a bio: increments the in-flight count only if the
    /// device is in the Running phase.
    ///
    /// Returns [`BioEnqueueError::Refused`] if the device is not Running
    /// (suspending or suspended); the caller then either defers the bio or
    /// completes it with an error if the device is being freed. On success
    /// the bio is counted as in-flight and a matching [`finish`](Self::finish)
    /// is required.
    fn submit(&self) -> Result<(), BioEnqueueError> {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            if cur & PHASE_MASK != PHASE_RUNNING {
                return Err(BioEnqueueError::Refused);
            }
            let new = cur.wrapping_add(1);
            // The in-flight count must never overflow into the phase bits.
            if new & PHASE_MASK != PHASE_RUNNING {
                return Err(BioEnqueueError::Refused);
            }
            if self
                .state
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Called when a bio completes (success or error). Wakes the drain
    /// waiter when the last in-flight bio finishes.
    fn finish(&self) {
        let prev = self.state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            prev & IN_FLIGHT_MASK > 0,
            "finish called with no in-flight bio"
        );
        if (prev & IN_FLIGHT_MASK) == 1 {
            self.drained.wake_all();
        }
    }

    /// Blocks until no in-flight (already-submitted) I/O remains.
    ///
    /// Deferred bios are **not** counted as in-flight: they have not been
    /// submitted to any target yet and are held for replay on resume.
    fn drain(&self) {
        self.drained.wait_until(|| {
            (self.state.load(Ordering::Acquire) & IN_FLIGHT_MASK == 0).then_some(())
        });
    }

    /// Transitions Running -> Suspending. New bios are now deferred instead
    /// of being submitted.
    fn mark_suspending(&self) {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            debug_assert_eq!(cur & PHASE_MASK, PHASE_RUNNING);
            let new = (cur & IN_FLIGHT_MASK) | PHASE_SUSPENDING;
            if self
                .state
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Transitions Suspending -> Suspended. The phase change is purely
    /// informational for drain callers; bios continue to be deferred.
    fn mark_suspended(&self) {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            debug_assert_eq!(cur & PHASE_MASK, PHASE_SUSPENDING);
            let new = (cur & IN_FLIGHT_MASK) | PHASE_SUSPENDED;
            if self
                .state
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Transitions to Running so that bios are admitted again. Called after
    /// the table swap on resume.
    fn clear_suspended(&self) {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            let new = cur & IN_FLIGHT_MASK;
            if self
                .state
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Forcefully sets the device to the Suspended phase, bypassing the
    /// normal Running→Suspending→Suspended transition. Used by `reset()`
    /// to restore a freshly-created device state without the assertion
    /// guards of `mark_suspending`/`mark_suspended`.
    fn force_suspended(&self) {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            let new = (cur & IN_FLIGHT_MASK) | PHASE_SUSPENDED;
            if self
                .state
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Returns whether the device is in the Suspending or Suspended phase.
    fn is_suspended(&self) -> bool {
        let phase = self.state.load(Ordering::Acquire) & PHASE_MASK;
        phase != PHASE_RUNNING
    }

    /// Defers a bio for replay on resume.
    ///
    /// Returns `Ok(())` if the bio was queued, or `Err(bio)` (returning
    /// ownership) if the device is being freed, in which case the caller
    /// must complete the bio with an error.
    fn push_deferred(&self, bio: SubmittedBio) -> Result<(), SubmittedBio> {
        if self.freeing.load(Ordering::Acquire) {
            return Err(bio);
        }
        self.deferred.lock().push_back(bio);
        Ok(())
    }

    /// Takes all deferred bios for replay.
    fn take_deferred(&self) -> VecDeque<SubmittedBio> {
        core::mem::take(&mut *self.deferred.lock())
    }

    /// Marks the device as being freed and completes all deferred bios with
    /// an error, so no bio is left pending after destruction.
    fn fail_deferred(&self) {
        self.freeing.store(true, Ordering::Release);
        for bio in self.take_deferred() {
            bio.complete(BioStatus::IoError);
        }
    }

    /// Returns the number of bios currently in flight.
    #[cfg(ktest)]
    fn in_flight(&self) -> usize {
        (self.state.load(Ordering::Acquire) & IN_FLIGHT_MASK) as usize
    }

    /// Returns the number of deferred bios (for tests).
    #[cfg(ktest)]
    fn deferred_len(&self) -> usize {
        self.deferred.lock().len()
    }
}

impl MappedDevice {
    /// Creates a new mapped device with the given table and resumes it
    /// immediately. This is the convenience kernel API.
    ///
    /// For the ioctl flow, use [`create_empty`](Self::create_empty) followed
    /// by [`load_table`](Self::load_table) and [`set_suspended`](Self::set_suspended).
    pub fn create(name: impl Into<Arc<str>>, table: DmTable) -> Result<Arc<Self>, DmError> {
        if table.num_targets() == 0 {
            return Err(DmError::InvalidTable(TableError::Empty));
        }
        let num_targets = table.num_targets();
        let total_sectors = table.total_sectors();
        let device = Self::create_empty(name)?;
        device.load_table(table)?;
        device.set_suspended(false);
        ostd::info!(
            "created mapped device '{}' (id={:?}) with {} target(s), {} sectors",
            device.name(),
            device.id,
            num_targets,
            total_sectors,
        );
        Ok(device)
    }

    /// Creates a new mapped device with no table.
    ///
    /// The device is registered with the block device registry and tracked
    /// by name. Call [`load_table`](Self::load_table) to provide a mapping
    /// table. The first `load_table` on a device with no active table loads
    /// directly to the active slot.
    pub fn create_empty(name: impl Into<Arc<str>>) -> Result<Arc<Self>, DmError> {
        manager().create(name, None::<Arc<str>>, None)
    }

    /// Creates a new mapped device with a caller-provided `DeviceId`.
    ///
    /// Unlike [`create_empty`](Self::create_empty), this method does **not**
    /// allocate a minor number or insert into the manager's registry.
    /// The caller (typically `DmManager`) is responsible for minor allocation
    /// and registry management.
    ///
    /// The device is created with no table and is
    /// registered with the block device layer via `aster_block::register`.
    pub fn new_with_id(
        id: DeviceId,
        name: Arc<str>,
        uuid: Option<Arc<str>>,
    ) -> Result<Arc<Self>, DmError> {
        let device = Arc::new(Self {
            id,
            name: spin::Mutex::new(name),
            display_name: format!("dm-{}", id.minor().get()),
            uuid: spin::Mutex::new(uuid.unwrap_or_else(|| Arc::from(""))),
            table: spin::Mutex::new(None),
            inactive_table: spin::Mutex::new(None),
            // Linux creates DM devices in the suspended state, but we don't:
            // libdevmapper may skip the resume ioctl when its internal
            // suspended-device counter is zero, which would strand I/O.
            // The suspended flag lives in DmIoState (cleared at construction).
            readonly: AtomicBool::new(false),
            io: Arc::new(DmIoState::new()),
            suspend_lock: spin::Mutex::new(()),
            event_nr: AtomicU32::new(0),
            event_wq: WaitQueue::new(),
            open_count: AtomicU32::new(0),
            deferred_remove: AtomicBool::new(false),
        });

        aster_block::register(device.clone()).map_err(|_| DmError::AlreadyRegistered)?;

        Ok(device)
    }

    /// Loads a target table into the device's inactive slot.
    ///
    /// The new table does not take effect immediately. It is swapped to the
    /// active slot when the device is resumed via
    /// [`set_suspended`](Self::set_suspended)`(false)`. This implements
    /// Linux's table hot-replacement semantics:
    /// suspend → load new table → resume swaps.
    ///
    /// If an inactive table already exists, it is overwritten (matching
    /// Linux's behavior where repeated `DM_TABLE_LOAD` calls replace the
    /// pending inactive table).
    pub fn load_table(&self, table: DmTable) -> Result<(), DmError> {
        // Always load to the inactive slot. The table is swapped to the
        // active slot on resume (DM_DEV_SUSPEND without DM_SUSPEND_FLAG).
        // This matches Linux's behavior and ensures the resume ioctl is
        // required for activation, which keeps libdevmapper's
        // `_suspended_dev_counter` in sync.
        *self.inactive_table.lock() = Some(Arc::new(table));
        // Note: loading a table does NOT bump the per-device event number.
        // In Linux, `md->event_nr` is incremented only by target-originated
        // events (`dm_table_event`), never by generic control operations.
        Ok(())
    }

    /// Clears the inactive table.
    ///
    /// Called by `DM_TABLE_CLEAR`, which LVM2 uses during deactivation
    /// (`lvchange -an`): suspend → clear inactive table → remove device.
    pub fn clear_table(&self) {
        *self.inactive_table.lock() = None;
    }

    /// Sets the suspended state of this device.
    ///
    /// When suspended, all I/O requests are refused with
    /// [`BioEnqueueError::Refused`].
    ///
    /// When resuming (`suspended = false`), if an inactive table was loaded,
    /// it is swapped to the active slot. This implements Linux's table
    /// hot-replacement.
    pub fn set_suspended(&self, suspended: bool) {
        self.set_suspended_with_flush(suspended, true);
    }

    /// Sets the suspended state without flushing in-flight I/O.
    ///
    /// This is used when userspace sets `DM_NOFLUSH_FLAG` during a suspend
    /// request, indicating that the caller does not need to wait for
    /// outstanding I/O to complete.
    pub fn set_suspended_no_flush(&self, suspended: bool) {
        self.set_suspended_with_flush(suspended, false);
    }

    fn set_suspended_with_flush(&self, suspended: bool, flush: bool) {
        // Serialize all suspend/resume transitions on this device.
        let _guard = self.suspend_lock.lock();

        if suspended {
            // Running -> Suspending: new bios are now deferred instead of
            // being submitted against the active table.
            self.io.mark_suspending();
            // A flush suspend waits for already-submitted bios to complete
            // so the table quiesces before the caller inspects it. A noflush
            // suspend returns immediately: in-flight bios keep running
            // against the old table (kept alive by their completion Arcs).
            if flush {
                self.io.drain();
            }
            // Suspending -> Suspended.
            self.io.mark_suspended();
        } else {
            // Resume: do NOT drain. Old in-flight bios keep completing
            // against the previous active table, whose Arc they hold. We
            // only need to (a) swap in the inactive table and (b) replay
            // the bios that were deferred while suspended.
            //
            // 1. Swap the inactive table to active if one was loaded.
            //    Lock order: table -> inactive_table.
            let mut active = self.table.lock();
            let mut inactive = self.inactive_table.lock();
            if let Some(new_table) = inactive.take() {
                *active = Some(new_table);
            }
            drop(inactive);
            drop(active);

            // 2. Back to Running first: newly-arriving bios are now admitted
            //    and dispatched against the (possibly new) active table. We
            //    clear the phase BEFORE draining the deferred queue so that
            //    any bio arriving in the window goes through the normal
            //    submit path (and uses the new table) instead of being
            //    pushed into a queue we just emptied.
            self.io.clear_suspended();

            // 3. Take and replay the deferred bios against the new active
            //    table. Each deferred bio must be re-admitted (submit) and
            //    then mapped. clear_suspended() above guarantees submit()
            //    succeeds here.
            let deferred = self.io.take_deferred();
            for bio in deferred {
                if self.io.submit().is_ok() {
                    let _ = self.map_submitted_bio(bio);
                } else {
                    bio.complete(BioStatus::IoError);
                }
            }
        }
    }

    /// Returns whether the device is currently suspended.
    pub fn is_suspended(&self) -> bool {
        self.io.is_suspended()
    }

    /// Returns the current event number.
    pub fn event_nr(&self) -> u32 {
        self.event_nr.load(Ordering::Acquire)
    }

    /// Bumps the event number and wakes any waiters.
    ///
    /// Only device-destroy and target-originated events call this; generic
    /// control operations (load/clear/suspend/resume) do not, matching
    /// Linux's `md->event_nr` semantics.
    pub fn bump_event_nr(&self) {
        self.event_nr.fetch_add(1, Ordering::AcqRel);
        self.event_wq.wake_all();
    }

    /// Waits until the event number differs from `last`.
    ///
    /// Uses inequality rather than "greater than" so the wait is correct
    /// after the `u32` counter wraps around, matching Linux's
    /// `dm_wait_event()` (`event_nr != atomic_read(&md->event_nr)`).
    ///
    /// This is an uninterruptible wait; callers needing signal-aware waiting
    /// (e.g. `DM_DEV_WAIT`) must use [`Self::event_wait_queue`] together with
    /// the `Pause` trait.
    pub fn wait_event(&self, last: u32) {
        self.event_wq
            .wait_until(|| (self.event_nr.load(Ordering::Acquire) != last).then_some(()));
    }

    /// Returns a reference to the per-device event wait queue.
    ///
    /// Exposed so the kernel-side ioctl handler (in `device::dm::control`)
    /// can drive an interruptible wait via the `Pause` trait, matching Linux's
    /// `dm_wait_event` which is interruptible by signals. Callers in the same
    /// crate as `Pause` should prefer `pause_until` over `wait_until` to allow
    /// `EINTR` return on signal delivery.
    pub fn event_wait_queue(&self) -> &WaitQueue {
        &self.event_wq
    }

    /// Resets the device to a "freshly created" state: no tables, not suspended.
    ///
    /// The device stays registered in the block layer; its name, minor, and
    /// identity are preserved. This is used by `DM_DEV_REMOVE` and by
    /// `DM_DEV_CREATE` on an already-registered device (LVM2's
    /// lvextend/lvreduce flow): the reused `Arc` keeps VFS inode caches
    /// valid across remove/create cycles, and the subsequent
    /// `DM_TABLE_LOAD` writes directly to the active slot since no active
    /// table remains after reset.
    pub fn reset(&self) {
        *self.table.lock() = None;
        *self.inactive_table.lock() = None;
        // Drain and fail any bios deferred during a prior suspend; the table
        // is gone so they cannot be replayed. Unlike `fail_deferred`, this
        // does NOT set the `freeing` flag, because `reset` is used to reuse
        // an existing device (e.g. DM_DEV_CREATE on an AlreadyRegistered
        // device) — new bios arriving after reset must be deferred normally
        // (device is in Suspended phase) rather than immediately failed.
        for bio in self.io.take_deferred() {
            bio.complete(BioStatus::IoError);
        }
        // Return to the Suspended phase, matching Linux's behavior where
        // a freshly created (or reset) device is suspended until an explicit
        // resume. This keeps libdevmapper's `_suspended_dev_counter` in sync.
        self.io.force_suspended();
    }

    /// Returns the device name.
    pub fn name(&self) -> Arc<str> {
        self.name.lock().clone()
    }

    /// Returns whether the device is marked read-only.
    pub fn is_readonly(&self) -> bool {
        self.readonly.load(Ordering::Acquire)
    }

    /// Marks the device as read-only or read-write.
    pub fn set_readonly(&self, readonly: bool) {
        self.readonly.store(readonly, Ordering::Release);
    }

    /// Returns the current open count.
    ///
    /// The count is maintained by the block-layer `open`/`close` hooks and
    /// is reported to userspace in ioctl status responses.
    pub fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }

    /// Marks the device for deferred removal.
    ///
    /// Used by `DM_DEV_REMOVE` with `DM_DEFERRED_REMOVE` when the device is
    /// busy: the removal is completed by the last `close()`.
    pub fn mark_deferred_remove(&self) {
        self.deferred_remove.store(true, Ordering::Release);
    }

    /// Returns whether a deferred removal is pending.
    pub fn is_deferred_remove_pending(&self) -> bool {
        self.deferred_remove.load(Ordering::Acquire)
    }

    /// Returns the number of bios currently in flight.
    #[cfg(ktest)]
    pub(crate) fn in_flight(&self) -> usize {
        self.io.in_flight()
    }

    /// Returns the number of bios deferred while suspended (tests only).
    #[cfg(ktest)]
    pub(crate) fn deferred_len(&self) -> usize {
        self.io.deferred_len()
    }

    /// Renames the device, updating its in-memory name field.
    ///
    /// This only updates the device's internal name. The caller is
    /// responsible for updating the manager's registry and any
    /// devtmpfs nodes via [`rename_by_name`](Self::rename_by_name).
    pub fn rename(&self, new_name: impl Into<Arc<str>>) {
        *self.name.lock() = new_name.into();
    }

    /// Atomically renames a registered device from `old_name` to `new_name`.
    ///
    /// This updates both the device's internal name field and the registry
    /// key, so subsequent lookups by the new name succeed. Returns the
    /// renamed device on success.
    ///
    /// Errors:
    /// - [`DmError::NotFound`] - no device registered under `old_name`.
    /// - [`DmError::AlreadyRegistered`] - `new_name` is already in use.
    pub fn rename_by_name(old_name: &str, new_name: &str) -> Result<Arc<Self>, DmError> {
        manager().rename(old_name, new_name)
    }

    /// Returns the device UUID (empty if none was set).
    pub fn uuid(&self) -> Arc<str> {
        self.uuid.lock().clone()
    }

    /// Sets the device UUID and updates the global UUID index.
    ///
    /// This is called from `DM_DEV_CREATE` when userspace (e.g. LVM2)
    /// supplies a UUID for the new device. The UUID is used for
    /// `lookup_by_uuid`, allowing subsequent ioctls to identify the device
    /// by UUID instead of by name.
    ///
    /// Returns an error if `uuid` is already used by another device.
    pub fn set_uuid(self: &Arc<Self>, uuid: &str) -> Result<(), DmError> {
        let old = self.uuid();
        manager().update_uuid(&self.name(), &old, uuid)
    }

    /// Directly sets the device UUID without touching the registry index.
    ///
    /// This is intended for the registry manager, which updates the index
    /// and the device field atomically under one lock.
    pub(crate) fn set_uuid_unchecked(&self, uuid: Arc<str>) {
        *self.uuid.lock() = uuid;
    }

    /// Returns the device ID.
    pub fn device_id(&self) -> DeviceId {
        self.id
    }

    /// Returns a clone of the current (active) table, if any.
    pub fn table(&self) -> Option<Arc<DmTable>> {
        self.table.lock().clone()
    }

    /// Routes a `DM_TARGET_MSG` to the target covering `sector` in the
    /// active table.
    ///
    /// Returns `DmError::NotFound` if there is no active table or no target
    /// covers the given sector.
    pub fn target_message(&self, sector: u64, message: &str) -> Result<(), DmError> {
        let table = self.table.lock();
        let table = table.as_ref().ok_or(DmError::NotFound)?;
        let target = table.find_target(sector).ok_or(DmError::NotFound)?;
        target.message(sector, message)
    }

    /// Returns a clone of the inactive table, if any.
    pub fn inactive_table(&self) -> Option<Arc<DmTable>> {
        self.inactive_table.lock().clone()
    }

    /// Returns a snapshot of both active and inactive tables while holding
    /// each lock only once.
    ///
    /// This is intended for callers like `DM_DEV_STATUS` that need a
    /// consistent view of both tables without paying the cost of multiple
    /// independent lock acquisitions.
    pub fn table_states(&self) -> (Option<Arc<DmTable>>, Option<Arc<DmTable>>) {
        let active = self.table.lock().clone();
        let inactive = self.inactive_table.lock().clone();
        (active, inactive)
    }

    // ------------------------------------------------------------------
    // Registry operations
    // ------------------------------------------------------------------

    /// Looks up a mapped device by name.
    pub fn lookup_by_name(name: &str) -> Option<Arc<MappedDevice>> {
        manager().lookup_name(name)
    }

    /// Looks up a mapped device by its UUID.
    ///
    /// LVM2 uses UUIDs of the form `LVM-<vg_uuid><lv_uuid>` to identify its
    /// logical volumes when issuing `DM_DEV_STATUS`, `DM_DEV_SUSPEND`, and
    /// `DM_DEV_REMOVE` ioctls. Without UUID-based lookup, LVM2 cannot find
    /// the devices it just created, causing `lvcreate` to fail with
    /// "No such file or directory" during activation.
    pub fn lookup_by_uuid(uuid: &str) -> Option<Arc<MappedDevice>> {
        manager().lookup_uuid(uuid)
    }

    /// Looks up a mapped device by its encoded device ID (glibc dev_t format).
    pub fn lookup_by_dev(dev: u64) -> Option<Arc<MappedDevice>> {
        let id = DeviceId::from_encoded_u64(dev)?;
        manager().lookup_id(id)
    }

    /// Removes a mapped device by name, unregistering it from the block
    /// device registry and recycling its minor number.
    ///
    /// Returns [`DmError::DeviceBusy`] if the device still has outstanding
    /// leases (e.g., a filesystem is mounted on it).
    pub fn remove_by_name(name: &str) -> Result<Arc<MappedDevice>, DmError> {
        manager().remove(name)
    }

    /// Returns a list of all registered mapped devices as `(name, uuid, id)`
    /// triples.
    ///
    /// The UUID is an empty `Arc<str>` if the device has no UUID set.
    pub fn list_devices() -> Vec<(Arc<str>, Arc<str>, DeviceId)> {
        manager().list_devices()
    }
}

impl MappedDevice {
    /// Maps an already-admitted bio against the current active table.
    ///
    /// The caller must have successfully called [`DmIoState::submit`] first
    /// (so the bio is counted in-flight). This method looks up the active
    /// table and attaches a completion closure that holds an `Arc` to it —
    /// the **table-keepalive barrier**: even if the table is swapped out
    /// during a noflush resume while this bio is in flight, the old table
    /// stays alive until the bio completes, so target callbacks never touch
    /// a freed table.
    ///
    /// Used both by [`enqueue`](BlockDevice::enqueue) and by resume to
    /// replay deferred bios.
    fn map_submitted_bio(&self, mut bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        let table = self.table.lock().clone();
        match table {
            Some(table) => {
                let io = self.io.clone();
                let table_keepalive = table.clone();
                bio.chain_complete_fn(move |_status| {
                    io.finish();
                    drop(table_keepalive);
                });
                match table.map_bio(bio) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        // map_bio failed: the completion closure above will
                        // never run (the bio was not submitted downstream),
                        // so finish the in-flight count manually.
                        self.io.finish();
                        Err(e)
                    }
                }
            }
            None => {
                self.io.finish();
                Err(BioEnqueueError::Refused)
            }
        }
    }

    /// Completes all deferred bios with an error. Called when the device is
    /// being removed so that no bio is left waiting on a table that no
    /// longer exists.
    pub fn fail_deferred_bios(&self) {
        self.io.fail_deferred();
    }

    /// Completes a deferred removal registered by `DM_DEV_REMOVE` with
    /// `DM_DEFERRED_REMOVE`.
    ///
    /// Called by `close()` when the open count reaches zero while the
    /// deferred-remove flag is set. Performs the same teardown as an
    /// immediate `DM_DEV_REMOVE`: unregisters the device from the DM and
    /// block registries (recycling the minor), bumps the event number so
    /// `DM_DEV_WAIT` sleepers wake up, fails any deferred bios, and invokes
    /// the registered hook (devtmpfs node cleanup).
    fn remove_deferred(&self) {
        let name = self.name();
        match manager().remove(&name) {
            Ok(removed) => {
                removed.bump_event_nr();
                removed.fail_deferred_bios();
                if let Some(hook) = DEFERRED_REMOVE_HOOK.get() {
                    hook(&name, removed.device_id());
                }
            }
            Err(_) => {
                // The device is still busy at the block layer (e.g. a
                // nested DM target holds a lease on it). Restore the flag
                // so that a later `DM_DEV_REMOVE` completes the removal.
                self.deferred_remove.store(true, Ordering::Release);
                ostd::warn!(
                    "deferred remove of '{}' could not complete yet; device kept",
                    name
                );
            }
        }
    }
}

impl BlockDevice for MappedDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        // Enforce the read-only flag. Reads and flushes are allowed; every
        // write-like request is refused.
        if self.is_readonly() && bio.type_().is_write_like() {
            return Err(BioEnqueueError::Refused);
        }

        // Atomically admit the bio only if the device is Running. If it is
        // suspending or suspended, submit() fails and we defer the bio
        // instead of returning Refused (which would surface as a spurious
        // EIO to the filesystem). On resume the deferred bio is replayed
        // against the active table — matching Linux's `md->deferred`.
        match self.io.submit() {
            Ok(()) => self.map_submitted_bio(bio),
            Err(_) => match self.io.push_deferred(bio) {
                Ok(()) => Ok(()),
                // Device is being freed: complete the bio with an error
                // rather than leaving it pending.
                Err(bio) => {
                    bio.complete(BioStatus::IoError);
                    Ok(())
                }
            },
        }
    }

    fn metadata(&self) -> BlockDeviceMeta {
        let table = self.table.lock().clone();
        match table {
            Some(table) => table.metadata(),
            None => BlockDeviceMeta {
                max_nr_segments_per_bio: usize::MAX,
                nr_sectors: 0,
            },
        }
    }

    fn name(&self) -> &str {
        // Return the stable `dm-<minor>` identifier.
        //
        // This identifier is derived from the device ID and stays the same
        // after `DM_DEV_RENAME`, so it can be returned as `&str` from
        // `BlockDevice::name()`.
        &self.display_name
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn open(&self) -> Result<(), aster_block::Error> {
        self.open_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn close(&self) {
        // Guard against an unbalanced close wrapping the counter to
        // `u32::MAX`; a saturating decrement keeps the count at zero.
        let Ok(prev) = self
            .open_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
        else {
            return;
        };
        // The last close with a deferred removal pending completes the
        // removal now, synchronously in the closer's context (no workqueue
        // is needed). The flag is cleared first so that the removal runs at
        // most once; it is restored if the device turns out to be busy.
        if prev == 1 && self.deferred_remove.swap(false, Ordering::AcqRel) {
            self.remove_deferred();
        }
    }
}

impl core::fmt::Debug for MappedDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("MappedDevice")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("suspended", &self.io.is_suspended())
            .finish_non_exhaustive()
    }
}

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    let major = DM_MAJOR
        .call_once(|| {
            Arc::new(
                allocate_major_with_name("device-mapper")
                    .expect("failed to allocate a major ID for device mapper"),
            )
        })
        .clone();
    DM_MANAGER.call_once(|| Arc::new(DmManager::new(major)));
    Ok(())
}

#[init_component(process)]
fn init_in_first_process() -> Result<(), ComponentInitError> {
    let create_args = DM_CREATE_ARGS.get().cloned().unwrap_or_default();
    if !create_args.is_empty() {
        ostd::info!(
            "creating {} dm device(s) from dm_mod.create=",
            create_args.len()
        );
    }
    parser::create_boot_devices(&create_args, false);

    let create_mandatory_args = DM_CREATE_MANDATORY_ARGS.get().cloned().unwrap_or_default();
    if !create_mandatory_args.is_empty() {
        ostd::info!(
            "creating {} mandatory dm device(s) from dm_mod.create_mandatory=",
            create_mandatory_args.len()
        );
    }
    parser::create_boot_devices(&create_mandatory_args, true);
    Ok(())
}
