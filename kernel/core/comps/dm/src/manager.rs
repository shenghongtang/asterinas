// SPDX-License-Identifier: MPL-2.0

//! Device mapper instance manager.
//!
//! This module provides [`DmManager`], an instance-centered DM device registry
//! that encapsulates name/UUID/ID triple indexing, RAII minor allocation, and
//! concurrency-safe create/remove/lookup operations.
//!
//! # Relationship with the global registry in `lib.rs`
//!
//! `DmManager` now backs the global device mapper registry. The public
//! functions in `crate` (such as [`crate::create_empty`]) delegate to a global
//! `DmManager` instance, so the rest of the kernel sees the same triple-index
//! registry and RAII minor management as a unit-test-local `DmManager`.
//!
//! # Design highlights
//!
//! - **Triple index**: `by_name`, `by_uuid`, and `by_dev`. All lookups run in
//!   O(log n).
//! - **Minor RAII**: [`MinorGuard`] returns its minor to the allocator on
//!   `Drop`, ensuring minor numbers are recycled when devices are destroyed.
//! - **Concurrency safety**: `inner` is protected by an `RwLock`. Public
//!   read-only methods (`lookup_*`, `list`) take the read lock; mutating
//!   methods (`create`, `remove`, `rename`, `update_uuid`) take the write lock.
//!
//! # Lock ordering
//!
//! To avoid deadlocks, the locking order is:
//!
//! 1. `inner` registry lock (read or write).
//! 2. Per-device `spin::Mutex` fields (`name`, `uuid`, etc.).
//!
//! `DmManager` never acquires per-device locks while holding the registry write
//! lock except for brief index computations that do not call back into the
//! registry.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use aster_block::MajorIdOwner;
use device_id::{DeviceId, MinorId};
use id_alloc::IdAlloc;
use ostd::sync::RwLock;
use spin::Mutex;

use crate::{DmError, MappedDevice, registry::Registry};

/// RAII guard for a minor number; returns it to the allocator on `Drop`.
///
/// Ensures minor numbers are recycled when devices are removed from the
/// manager, even if a panic occurs. Created by [`DmManager::create`] and
/// dropped by [`DmManager::remove`].
struct MinorGuard {
    /// The minor number this guard owns.
    minor: u32,
    /// Shared reference to the minor allocator.
    alloc: Arc<Mutex<IdAlloc>>,
}

impl MinorGuard {
    /// Creates a new minor guard.
    fn new(minor: u32, alloc: Arc<Mutex<IdAlloc>>) -> Self {
        Self { minor, alloc }
    }
}

impl Drop for MinorGuard {
    fn drop(&mut self) {
        // Return the minor to the allocator to prevent leaks.
        // Even on abnormal destruction the minor is correctly recycled.
        self.alloc.lock().free(self.minor as usize);
    }
}

/// Device mapper manager responsible for the full lifecycle of DM devices.
///
/// `DmManager` holds:
/// - One block major number shared by all DM devices.
/// - A minor number allocator (`IdAlloc`, supporting both automatic and
///   explicit allocation).
/// - A device registry ([`Registry`], name/UUID/ID triple index).
/// - A minor guard table (`MinorGuard` RAII map).
///
/// # Concurrency model
///
/// The registry is protected by an `RwLock`. Minor allocation uses a separate
/// `minors` mutex. The public lock ordering is always `minors` (only during
/// creation) followed by `inner`. `remove` drops the write lock before calling
/// into the block layer to unregister the device, which avoids holding the
/// registry lock during a potentially blocking operation and prevents
/// deadlocks with the block layer.
pub struct DmManager {
    /// Shared ownership of the block major number.
    major: Arc<MajorIdOwner>,
    /// Minor number allocator shared by all DM devices.
    minors: Arc<Mutex<IdAlloc>>,
    /// Registry inner state, protected by an `RwLock`.
    inner: RwLock<Registry>,
    /// RAII minor guard table, keyed by minor number.
    /// When a device is removed from the registry, the corresponding
    /// `MinorGuard` is dropped, automatically returning the minor.
    guards: Mutex<BTreeMap<u32, MinorGuard>>,
}

impl DmManager {
    /// Creates a new `DmManager`.
    ///
    /// # Arguments
    /// - `major`: an already-allocated block major number owner.
    ///
    /// # Returns
    /// A new `DmManager` instance whose minor allocator is initialized with
    /// `0..=MinorId::MAX` all available.
    pub fn new(major: Arc<MajorIdOwner>) -> Self {
        Self {
            major,
            minors: Arc::new(Mutex::new(IdAlloc::with_capacity(
                MinorId::MAX.get() as usize + 1,
            ))),
            inner: RwLock::new(Registry::new()),
            guards: Mutex::new(BTreeMap::new()),
        }
    }

    /// Creates a new `DmManager` with a limited minor pool capacity.
    ///
    /// Test-only constructor that allows exercising minor exhaustion and
    /// recycling without allocating `MinorId::MAX + 1` devices.
    #[cfg(ktest)]
    pub fn with_minor_capacity(major: Arc<MajorIdOwner>, capacity: usize) -> Self {
        Self {
            major,
            minors: Arc::new(Mutex::new(IdAlloc::with_capacity(capacity))),
            inner: RwLock::new(Registry::new()),
            guards: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the block major number used by this manager.
    pub fn major(&self) -> device_id::MajorId {
        self.major.get()
    }

    /// Creates a new DM device and registers it with the manager.
    ///
    /// # Arguments
    /// - `name`: device name, must be unique in the registry.
    /// - `uuid`: optional device UUID; if provided, must also be unique.
    /// - `requested_minor`: optional explicit minor number. `None` means auto.
    ///
    /// # Errors
    /// - `AlreadyRegistered`: the name already exists.
    /// - `UuidExists`: the UUID already exists.
    /// - `MinorBusy`: the requested minor is already in use.
    /// - `MinorExhausted`: the minor overflows `MinorId::MAX` or the pool is
    ///   exhausted.
    ///
    /// # Concurrency safety
    /// Minor allocation and block-layer registration happen outside the write lock;
    /// name/UUID uniqueness checks and registry insertion are performed while
    /// holding the write lock, preventing TOCTOU races. Duplicate entries roll
    /// back the block registration and free the minor.
    pub fn create(
        &self,
        name: impl Into<Arc<str>>,
        uuid: Option<impl Into<Arc<str>>>,
        requested_minor: Option<u32>,
    ) -> Result<Arc<MappedDevice>, DmError> {
        let name: Arc<str> = name.into();
        let uuid: Option<Arc<str>> = uuid.map(|u| u.into());

        // Allocate the minor first, before acquiring `inner`, to preserve the
        // documented lock order minors -> inner. If later uniqueness checks
        // fail we must free the minor to avoid leaking it.
        let minor = {
            let mut minors = self.minors.lock();
            match requested_minor {
                Some(minor) => {
                    // Explicit minor allocation, used by LVM2 restore.
                    let minor = usize::try_from(minor).map_err(|_| DmError::MinorExhausted)?;
                    if minor > MinorId::MAX.get() as usize {
                        return Err(DmError::MinorExhausted);
                    }
                    minors.alloc_specific(minor).ok_or(DmError::MinorBusy)?
                }
                None => minors.alloc().ok_or(DmError::MinorExhausted)?,
            }
        } as u32;

        let id = DeviceId::new(self.major.get(), MinorId::new(minor));
        // Register with the block layer before taking the registry write lock.
        // This avoids holding `inner` during a potentially blocking operation
        // and preserves the documented lock ordering.
        let device = MappedDevice::new_with_id(id, name.clone(), uuid.clone())?;

        let mut inner = self.inner.write();

        // Check name uniqueness.
        if inner.by_name.contains_key(name.as_ref()) {
            drop(inner);
            // The device was just registered above and cannot have any
            // leases yet, so unregistration cannot fail.
            debug_assert!(aster_block::unregister(id).is_ok());
            self.minors.lock().free(minor as usize);
            return Err(DmError::AlreadyRegistered);
        }

        // Check UUID uniqueness.
        if uuid
            .as_ref()
            .is_some_and(|uuid| !uuid.is_empty() && inner.by_uuid.contains_key(uuid.as_ref()))
        {
            drop(inner);
            // Same as above: the freshly registered device cannot be busy.
            debug_assert!(aster_block::unregister(id).is_ok());
            self.minors.lock().free(minor as usize);
            return Err(DmError::UuidExists);
        }

        // Create a minor guard to ensure the minor is recycled on removal.
        let guard = MinorGuard::new(minor, self.minors.clone());
        self.guards.lock().insert(minor, guard);

        // Register in the index tables.
        inner.insert(name, device.clone());

        Ok(device)
    }

    /// Removes a device from the manager.
    ///
    /// The device is first removed from the registry, then unregistered from
    /// the global block device registry. If the block layer reports that the
    /// device is busy, the device is restored to the registry and
    /// `DmError::DeviceBusy` is returned.
    ///
    /// # Arguments
    /// - `name`: the name of the device to remove.
    ///
    /// # Errors
    /// - `NotFound`: the device does not exist.
    /// - `DeviceBusy`: the device is still in use by the block layer.
    pub fn remove(&self, name: &str) -> Result<Arc<MappedDevice>, DmError> {
        let mut inner = self.inner.write();
        let device = inner.remove(name).ok_or(DmError::NotFound)?;
        let minor = device.device_id().minor().get();

        // Drop the registry write lock before unregistering with the block
        // layer. This avoids holding the registry lock during a potentially
        // blocking operation and prevents deadlocks.
        drop(inner);

        match aster_block::unregister(device.device_id()) {
            Ok(_) => {
                // Drop the minor guard, triggering RAII recycling.
                self.guards.lock().remove(&minor);
                Ok(device)
            }
            Err(err) => {
                // Restore the registry entry so the device stays manageable
                // and the caller can retry.
                let mut inner = self.inner.write();
                inner.insert(Arc::from(name), device.clone());
                // `unregister` only fails with `Busy` (outstanding leases)
                // or `NotFound` (entry missing from the block registry,
                // which cannot happen for a device registered at creation).
                debug_assert_eq!(err, aster_block::Error::Busy);
                Err(DmError::DeviceBusy)
            }
        }
    }

    /// Renames a registered device.
    ///
    /// The internal device name is updated; the stable display name
    /// (`BlockDevice::name()`, e.g. `dm-<minor>`) does not change.
    ///
    /// # Errors
    /// - `NotFound`: the source device does not exist.
    /// - `AlreadyRegistered`: the target name is already in use.
    pub fn rename(&self, old_name: &str, new_name: &str) -> Result<Arc<MappedDevice>, DmError> {
        let new_name: Arc<str> = Arc::from(new_name);
        let mut inner = self.inner.write();
        let device = inner.remove(old_name).ok_or(DmError::NotFound)?;

        if inner.by_name.contains_key(new_name.as_ref()) {
            // Restore the old mapping on conflict.
            inner.insert(Arc::from(old_name), device.clone());
            return Err(DmError::AlreadyRegistered);
        }

        *device.name.lock() = new_name.clone();
        inner.insert(new_name, device.clone());
        Ok(device)
    }

    /// Looks up a device by name.
    ///
    /// Returns a cloned `Arc` to the device, or `None` if not found.
    pub fn lookup_name(&self, name: &str) -> Option<Arc<MappedDevice>> {
        self.inner.read().by_name.get(name).cloned()
    }

    /// Looks up a device by UUID.
    ///
    /// Uses the `by_uuid` secondary index for an O(log n) lookup.
    /// Returns a cloned `Arc` to the device, or `None` if not found.
    pub fn lookup_uuid(&self, uuid: &str) -> Option<Arc<MappedDevice>> {
        if uuid.is_empty() {
            return None;
        }
        self.inner.read().by_uuid.get(uuid).cloned()
    }

    /// Looks up a device by device ID (major:minor).
    ///
    /// First verifies the major matches, then uses the `by_dev` secondary
    /// index for an O(log n) lookup.
    pub fn lookup_id(&self, id: DeviceId) -> Option<Arc<MappedDevice>> {
        if id.major() != self.major.get() {
            return None;
        }
        self.inner.read().by_dev.get(&id).cloned()
    }

    /// Updates a device's UUID index and the device's internal UUID field.
    ///
    /// Both updates happen while the registry write lock is held, so the
    /// index and the device stay consistent.
    ///
    /// # Arguments
    /// - `name`: the device name.
    /// - `old_uuid`: the old UUID (empty means the device had none).
    /// - `new_uuid`: the new UUID (empty means clear the UUID).
    ///
    /// # Errors
    /// - `NotFound`: the device does not exist.
    /// - `UuidExists`: the new UUID is already used by another device.
    pub fn update_uuid(&self, name: &str, old_uuid: &str, new_uuid: &str) -> Result<(), DmError> {
        let new_uuid: Arc<str> = Arc::from(new_uuid);
        let mut inner = self.inner.write();
        let device = inner.by_name.get(name).ok_or(DmError::NotFound)?.clone();

        // Check whether the new UUID is already taken by another device.
        let uuid_taken_by_other = !new_uuid.is_empty()
            && inner
                .by_uuid
                .get(new_uuid.as_ref())
                .is_some_and(|existing| !Arc::ptr_eq(existing, &device));
        if uuid_taken_by_other {
            return Err(DmError::UuidExists);
        }

        // If the UUID is unchanged, nothing to do.
        if old_uuid == new_uuid.as_ref() {
            return Ok(());
        }

        // Insert the new UUID mapping first, then update the device's
        // internal UUID, and finally remove the old mapping. This ordering
        // ensures that the new UUID is always resolvable while the device
        // transitions from the old UUID to the new one; there is no window
        // where `lookup_by_uuid(new_uuid)` returns None while the device
        // already reports the new UUID.
        if !new_uuid.is_empty() {
            inner.by_uuid.insert(new_uuid.clone(), device.clone());
        }
        device.set_uuid_unchecked(new_uuid);
        if !old_uuid.is_empty() {
            inner.by_uuid.remove(old_uuid);
        }

        Ok(())
    }

    /// Returns a snapshot list of all registered devices.
    ///
    /// The returned `Vec` is a cloned snapshot; the caller can iterate it
    /// safely without interference from concurrent registry modifications.
    pub fn devices(&self) -> Vec<Arc<MappedDevice>> {
        self.inner.read().by_name.values().cloned().collect()
    }

    /// Returns a list of all registered devices as `(name, uuid, id)` triples.
    ///
    /// The UUID is an empty `Arc<str>` if the device has no UUID set; callers
    /// writing `dm_name_list` entries decide whether to append the UUID tail
    /// (and set `DM_NAME_LIST_FLAG_HAS_UUID`) based on whether the string is
    /// non-empty.
    pub fn list_devices(&self) -> Vec<(Arc<str>, Arc<str>, DeviceId)> {
        self.inner
            .read()
            .by_name
            .iter()
            .map(|(name, dev)| (name.clone(), dev.uuid(), dev.device_id()))
            .collect()
    }
}
