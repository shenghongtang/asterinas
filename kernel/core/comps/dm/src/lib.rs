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
//! - Verity verification is synchronous.
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

use alloc::{collections::BTreeMap, format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aster_block::{
    BlockDevice, BlockDeviceMeta, MajorIdOwner, allocate_major_with_name,
    bio::{BioEnqueueError, BioType, SubmittedBio},
};
use component::{ComponentInitError, init_component};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::sync::{RwLock, WaitQueue};
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
pub use target::{DmTarget, Target};
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

/// Registry of supported device mapper target types and their versions.
static TARGET_VERSIONS: RwLock<BTreeMap<&'static str, [u32; 3]>> = RwLock::new(BTreeMap::new());

/// Registers a device mapper target type and its version.
pub fn register_target_type(name: &'static str, version: [u32; 3]) {
    TARGET_VERSIONS.write().insert(name, version);
}

/// Returns the list of supported target types and their versions, sorted by name.
pub fn list_target_versions() -> Vec<(&'static str, [u32; 3])> {
    TARGET_VERSIONS
        .read()
        .iter()
        .map(|(&name, &version)| (name, version))
        .collect()
}

/// Returns the version of the named target type, if registered.
pub fn get_target_version(name: &str) -> Option<[u32; 3]> {
    TARGET_VERSIONS.read().get(name).copied()
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
    /// Monotonically increasing event number for `DM_DEV_WAIT`.
    event_nr: AtomicU32,
    /// Wait queue woken when the event number increases.
    event_wq: WaitQueue,
    /// Open count reported to userspace via `DmIoctl::open_count`.
    ///
    /// Currently no block-layer open/close hook is available, so this
    /// remains zero until a tracking mechanism is added.
    open_count: AtomicU32,
}

/// Tracks in-flight I/O requests and the suspended state so that `suspend`
/// can wait for all outstanding bios to complete before switching tables.
///
/// The suspended flag and the in-flight count are packed into a single
/// `AtomicU32` so that [`submit`](Self::submit) can perform the
/// "admit only if not suspended" check together with the in-flight
/// increment as one atomic CAS. This closes the TOCTOU window where a bio
/// could pass a separate suspended check but not yet be counted when
/// [`drain`](Self::drain) runs, which would let I/O reach a table that has
/// already been swapped out.
struct DmIoState {
    /// Bit 31: suspended flag. Bits 0..=30: in-flight bio count.
    state: AtomicU32,
    drained: WaitQueue,
}

/// Mask for the suspended flag (bit 31) in [`DmIoState::state`].
const SUSPENDED_BIT: u32 = 1 << 31;
/// Mask for the in-flight count (bits 0..=30) in [`DmIoState::state`].
const IN_FLIGHT_MASK: u32 = !SUSPENDED_BIT;

impl DmIoState {
    fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            drained: WaitQueue::new(),
        }
    }

    /// Atomically admits a bio: increments the in-flight count only if the
    /// device is not suspended.
    ///
    /// Returns [`BioEnqueueError::Refused`] if the device is suspended, so
    /// the caller must not forward the bio. On success the bio is counted
    /// as in-flight and a matching [`finish`](Self::finish) is required.
    fn submit(&self) -> Result<(), BioEnqueueError> {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            if cur & SUSPENDED_BIT != 0 {
                return Err(BioEnqueueError::Refused);
            }
            let new = cur.wrapping_add(1);
            // The in-flight count must never overflow into the suspended bit.
            if new & SUSPENDED_BIT != 0 {
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

    /// Blocks until no in-flight I/O remains.
    ///
    /// The caller must have marked the device suspended first (via
    /// [`mark_suspended`](Self::mark_suspended)) so that no new bios are
    /// admitted while draining.
    fn drain(&self) {
        self.drained.wait_until(|| {
            (self.state.load(Ordering::Acquire) & IN_FLIGHT_MASK == 0).then_some(())
        });
    }

    /// Sets the suspended flag atomically. After this call, `submit` will
    /// refuse all new bios.
    fn mark_suspended(&self) {
        self.state.fetch_or(SUSPENDED_BIT, Ordering::AcqRel);
    }

    /// Clears the suspended flag atomically, allowing new bios to be
    /// admitted again.
    fn clear_suspended(&self) {
        self.state.fetch_and(IN_FLIGHT_MASK, Ordering::AcqRel);
    }

    /// Returns whether the suspended flag is set.
    fn is_suspended(&self) -> bool {
        self.state.load(Ordering::Acquire) & SUSPENDED_BIT != 0
    }

    /// Returns the number of bios currently in flight.
    #[cfg(ktest)]
    fn in_flight(&self) -> usize {
        (self.state.load(Ordering::Acquire) & IN_FLIGHT_MASK) as usize
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
            event_nr: AtomicU32::new(0),
            event_wq: WaitQueue::new(),
            open_count: AtomicU32::new(0),
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
        // For a newly created device with no active table, load directly to
        // active so I/O works immediately. This is necessary because
        // libdevmapper may skip the resume ioctl (DM_DEV_SUSPEND without
        // DM_SUSPEND_FLAG) when its internal suspended-device counter is zero,
        // leaving the table stranded in the inactive slot.
        let mut active = self.table.lock();
        if active.is_none() {
            *active = Some(Arc::new(table));
        } else {
            // Active table exists - load to inactive for hot replacement.
            drop(active);
            *self.inactive_table.lock() = Some(Arc::new(table));
        }
        self.bump_event_nr();
        Ok(())
    }

    /// Clears the inactive table.
    ///
    /// Called by `DM_TABLE_CLEAR`, which LVM2 uses during deactivation
    /// (`lvchange -an`): suspend → clear inactive table → remove device.
    pub fn clear_table(&self) {
        *self.inactive_table.lock() = None;
        self.bump_event_nr();
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
        if suspended {
            // Suspend: mark suspended first so that no new bios are admitted
            // (submit() atomically refuses them), then wait for all in-flight
            // bios to complete before returning unless the caller requested a
            // no-flush suspend.
            self.io.mark_suspended();
            if flush {
                self.io.drain();
            }
            self.bump_event_nr();
        } else {
            // Resume: drain any in-flight I/O that may still be using the
            // old active table (e.g. from a noflush suspend) before swapping
            // in the inactive table. `drain` is safe here because the device
            // is still suspended, so no new bios are admitted and the
            // in-flight count is guaranteed to reach zero. This ensures no
            // bio completes against a table that has been replaced.
            self.io.drain();

            // Swap the inactive table to active if one was loaded.
            // Lock order must be table -> inactive_table everywhere to avoid
            // deadlocks with reset(), table_states(), and update_dev_status().
            let mut active = self.table.lock();
            let mut inactive = self.inactive_table.lock();
            if let Some(new_table) = inactive.take() {
                *active = Some(new_table);
            }
            // Clear suspended only after the table swap so that bios admitted
            // after resume see the new (active) table, not the old one.
            self.io.clear_suspended();
            self.bump_event_nr();
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
    pub fn bump_event_nr(&self) {
        self.event_nr.fetch_add(1, Ordering::AcqRel);
        self.event_wq.wake_all();
    }

    /// Waits until the event number is greater than `last`.
    pub fn wait_event(&self, last: u32) {
        self.event_wq
            .wait_until(|| (self.event_nr.load(Ordering::Acquire) > last).then_some(()));
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
        self.io.clear_suspended();
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
    /// The value is always zero today because the block layer does not yet
    /// expose an open/close hook. It is exposed here so that ioctl status
    /// responses can report a consistent value when such tracking is added.
    pub fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }

    /// Returns the number of bios currently in flight.
    #[cfg(ktest)]
    pub(crate) fn in_flight(&self) -> usize {
        self.io.in_flight()
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

    /// Returns a list of all registered mapped devices as `(name, id)` pairs.
    pub fn list_devices() -> Vec<(Arc<str>, DeviceId)> {
        manager().list_devices()
    }
}

impl BlockDevice for MappedDevice {
    fn enqueue(&self, mut bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        // P0-2: Enforce the read-only flag. Reads and flushes are allowed;
        // writes are refused. Flush is permitted because a read-only device
        // may still need to flush already-persisted data, and filesystems
        // issue flush regardless of the device's writability.
        if self.is_readonly() && bio.type_() == BioType::Write {
            return Err(BioEnqueueError::Refused);
        }

        // P0-1: Atomically admit the bio. submit() checks the suspended flag
        // and increments the in-flight count in a single CAS, so a bio that
        // passes this point is guaranteed to be counted by a concurrent
        // suspend's drain(). This closes the TOCTOU window where a bio could
        // observe "not suspended" but not yet be in-flight when drain runs.
        self.io.submit()?;

        // Clone the table only after admission. Because the in-flight count
        // was incremented atomically with the suspended check, any suspend
        // that starts now will drain() and wait for this bio to finish
        // before swapping the table.
        let table = self.table.lock().clone();
        match table {
            Some(table) => {
                let io = self.io.clone();
                bio.chain_complete_fn(move |_status| {
                    io.finish();
                });
                match table.map_bio(bio) {
                    Ok(()) => Ok(()),
                    Err(e) => {
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
        self.open_count.fetch_sub(1, Ordering::AcqRel);
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
    targets::register_all();

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
