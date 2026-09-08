// SPDX-License-Identifier: MPL-2.0

//! The block devices of Asterinas.
//！
//！This crate provides a number of base components for block devices, including
//! an abstraction of block devices, as well as the registration and lookup of block devices.
//!
//! Block devices use a queue-based model for asynchronous I/O operations. It is necessary
//! for a block device to maintain a queue to handle I/O requests. The users (e.g., fs)
//! submit I/O requests to this queue and wait for their completion. Drivers implementing
//! block devices can create their own queues as needed, with the possibility to reorder
//! and merge requests within the queue.
//!
//! This crate also offers the `Bio` related data structures and APIs to accomplish
//! safe and convenient block I/O operations, for example:
//!
//! ```no_run
//! // Creates a bio request.
//! let bio = Bio::new(BioType::Write, sid, segments, None);
//! // Submits to the block device.
//! let mut io_batch = IoBatch::new();
//! bio.submit(block_device, &mut io_batch)?;
//! // Waits for the the completion.
//! io_batch.wait_all()?;
//! ```
//!
#![no_std]
#![deny(unsafe_code)]
#![feature(step_trait)]

extern crate alloc;
#[macro_use]
extern crate ostd_pod;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "block: "
    };
}

pub mod bio;
mod device_id;
pub mod id;
mod impl_block_device;
mod partition;
mod prelude;
pub mod request_queue;

use ::device_id::DeviceId;
use component::{ComponentInitError, init_component};
pub use device_id::{
    EXTENDED_DEVICE_ID_ALLOCATOR, MajorIdOwner, acquire_major, acquire_major_with_name,
    allocate_major, allocate_major_with_name, major_devices,
};
use ostd::sync::Mutex;
pub use partition::{PartitionInfo, PartitionNode};

use self::{
    bio::{BioEnqueueError, SubmittedBio},
    prelude::*,
};

pub const BLOCK_SIZE: usize = ostd::mm::PAGE_SIZE;
pub const SECTOR_SIZE: usize = 512;

pub trait BlockDevice: Send + Sync + Any + Debug {
    /// Enqueues a new `SubmittedBio` to the block device.
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError>;

    /// Returns the metadata of the block device.
    fn metadata(&self) -> BlockDeviceMeta;

    /// Returns the name of the block device.
    fn name(&self) -> &str;

    /// Returns the device ID of the block device.
    fn id(&self) -> DeviceId;

    /// Returns whether the block device is a partition.
    fn is_partition(&self) -> bool {
        false
    }

    /// Sets the partitions of the block device.
    fn set_partitions(&self, _infos: Vec<Option<PartitionInfo>>) {}

    /// Returns the partitions of the block device.
    fn partitions(&self) -> Option<Vec<Arc<dyn BlockDevice>>> {
        None
    }

    /// Called when the block device file is opened by userspace.
    ///
    /// Devices may use this to maintain an open counter (e.g. for `dmsetup
    /// info`) or to acquire resources. The default implementation is a no-op.
    fn open(&self) -> Result<(), Error> {
        Ok(())
    }

    /// Called when the last file descriptor referring to an open block device
    /// file is released.
    ///
    /// The default implementation is a no-op.
    fn close(&self) {}
}

/// Metadata for a block device.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockDeviceMeta {
    /// The upper limit for the number of segments per bio.
    pub max_nr_segments_per_bio: usize,
    /// The total number of sectors of the block device.
    pub nr_sectors: usize,
    // Additional useful metadata can be added here in the future.
}

impl dyn BlockDevice {
    pub fn downcast_ref<T: BlockDevice>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref::<T>()
    }
}

/// The error type which is returned from the APIs of this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Device registered
    Registered,
    /// Device not found
    NotFound,
    /// Invalid arguments
    InvalidArgs,
    /// Id Acquired
    IdAcquired,
    /// Id Exhausted
    IdExhausted,
    /// The device is still in use (lease held) or is transitioning state.
    Busy,
}

// ---------------------------------------------------------------------------
// Registration with lease support
// ---------------------------------------------------------------------------

/// Internal registration record that tracks the lifecycle status and the
/// number of outstanding [`BlockDeviceLease`]s.
#[derive(Debug)]
struct RegisteredBlockDevice {
    id: DeviceId,
    device: Arc<dyn BlockDevice>,
    state: Mutex<RegisteredBlockDeviceState>,
}

#[derive(Debug)]
struct RegisteredBlockDeviceState {
    status: RegistrationStatus,
    lease_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationStatus {
    Pending,
    Live,
    Removing,
}

/// A lease that prevents a registered block device from being unregistered
/// while it is in use (e.g., by a DM target or a mounted filesystem).
#[derive(Debug)]
pub struct BlockDeviceLease {
    device: Arc<dyn BlockDevice>,
    registered: Option<Arc<RegisteredBlockDevice>>,
}

impl BlockDeviceLease {
    /// Returns the underlying block device protected by this lease.
    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.device
    }

    /// Creates a lease that does not participate in registry lifecycle
    /// tracking. Intended for kernel tests that construct ad-hoc devices.
    #[cfg(ktest)]
    pub fn new_untracked(device: Arc<dyn BlockDevice>) -> Self {
        Self {
            device,
            registered: None,
        }
    }
}

impl core::ops::Deref for BlockDeviceLease {
    type Target = dyn BlockDevice;

    fn deref(&self) -> &Self::Target {
        self.device.as_ref()
    }
}

impl Clone for BlockDeviceLease {
    fn clone(&self) -> Self {
        if let Some(registered) = &self.registered {
            let mut state = registered.state.lock();
            debug_assert!(state.lease_count > 0);
            state.lease_count = state
                .lease_count
                .checked_add(1)
                .expect("block device lease count overflow");
        }

        Self {
            device: self.device.clone(),
            registered: self.registered.clone(),
        }
    }
}

impl Drop for BlockDeviceLease {
    fn drop(&mut self) {
        let Some(registered) = &self.registered else {
            return;
        };
        let mut state = registered.state.lock();
        debug_assert!(state.lease_count > 0);
        state.lease_count -= 1;
    }
}

/// A pending registration token for a block device not yet published to lookups.
#[derive(Debug)]
pub struct PendingBlockDeviceRegistration {
    registered: Arc<RegisteredBlockDevice>,
}

impl PendingBlockDeviceRegistration {
    /// Returns the device ID of the pending registration.
    pub fn id(&self) -> DeviceId {
        self.registered.id
    }
}

/// A pending unregistration token for a block device that has stopped
/// accepting new lookups but may still have outstanding leases.
#[derive(Debug)]
pub struct PendingBlockDeviceUnregistration {
    registered: Arc<RegisteredBlockDevice>,
}

impl PendingBlockDeviceUnregistration {
    /// Returns the device ID of the pending unregistration.
    pub fn id(&self) -> DeviceId {
        self.registered.id
    }
}

impl Drop for PendingBlockDeviceRegistration {
    fn drop(&mut self) {
        let mut registry = DEVICE_REGISTRY.lock();
        let id = self.id().to_raw();
        let Some(current) = registry.get(&id) else {
            return;
        };
        if !Arc::ptr_eq(current, &self.registered) {
            return;
        }
        let state = current.state.lock();
        if state.status != RegistrationStatus::Pending || state.lease_count != 0 {
            return;
        }
        drop(state);
        registry.remove(&id);
    }
}

impl Drop for PendingBlockDeviceUnregistration {
    fn drop(&mut self) {
        let registry = DEVICE_REGISTRY.lock();
        let Some(current) = registry.get(&self.id().to_raw()) else {
            return;
        };
        if !Arc::ptr_eq(current, &self.registered) {
            return;
        }
        let mut state = current.state.lock();
        if state.status == RegistrationStatus::Removing && state.lease_count == 0 {
            state.status = RegistrationStatus::Live;
        }
    }
}

/// Begins registering a block device without yet publishing it to lookups.
pub fn register_pending(
    device: Arc<dyn BlockDevice>,
) -> Result<PendingBlockDeviceRegistration, Error> {
    let id = device.id();
    let registered = Arc::new(RegisteredBlockDevice {
        id,
        device,
        state: Mutex::new(RegisteredBlockDeviceState {
            status: RegistrationStatus::Pending,
            lease_count: 0,
        }),
    });
    let mut registry = DEVICE_REGISTRY.lock();
    if registry.contains_key(&id.to_raw()) {
        return Err(Error::Registered);
    }
    registry.insert(id.to_raw(), registered.clone());

    Ok(PendingBlockDeviceRegistration { registered })
}

/// Publishes a pending block device registration.
pub fn commit_registration(registration: &PendingBlockDeviceRegistration) -> Result<(), Error> {
    let registry = DEVICE_REGISTRY.lock();
    let current = registry
        .get(&registration.id().to_raw())
        .ok_or(Error::NotFound)?;
    if !Arc::ptr_eq(current, &registration.registered) {
        return Err(Error::Registered);
    }

    let mut state = current.state.lock();
    if state.status != RegistrationStatus::Pending {
        return Err(Error::Busy);
    }
    state.status = RegistrationStatus::Live;
    Ok(())
}

/// Cancels a pending registration and removes the device from the registry.
pub fn abort_registration(
    registration: PendingBlockDeviceRegistration,
) -> Result<Arc<dyn BlockDevice>, Error> {
    let mut registry = DEVICE_REGISTRY.lock();
    let id = registration.id().to_raw();
    let current = registry.get(&id).ok_or(Error::NotFound)?;
    if !Arc::ptr_eq(current, &registration.registered) {
        return Err(Error::Registered);
    }

    let state = current.state.lock();
    if state.status != RegistrationStatus::Pending || state.lease_count != 0 {
        return Err(Error::Busy);
    }
    drop(state);

    let registered = registry.remove(&id).unwrap();
    Ok(registered.device.clone())
}

/// Registers a new block device (convenience: register_pending + commit).
pub fn register(device: Arc<dyn BlockDevice>) -> Result<(), Error> {
    let registration = register_pending(device)?;
    commit_registration(&registration)
}

/// Begins unregistering a published block device, preventing new lookups/leases.
pub fn begin_unregister(id: DeviceId) -> Result<PendingBlockDeviceUnregistration, Error> {
    let registry = DEVICE_REGISTRY.lock();
    let registered = registry.get(&id.to_raw()).ok_or(Error::NotFound)?;
    let mut state = registered.state.lock();
    if state.status != RegistrationStatus::Live || state.lease_count != 0 {
        return Err(Error::Busy);
    }
    state.status = RegistrationStatus::Removing;
    drop(state);

    Ok(PendingBlockDeviceUnregistration {
        registered: registered.clone(),
    })
}

/// Commits a pending unregistration, removing the device from the registry.
pub fn commit_unregister(
    unregistration: PendingBlockDeviceUnregistration,
) -> Result<Arc<dyn BlockDevice>, Error> {
    let mut registry = DEVICE_REGISTRY.lock();
    let id = unregistration.id().to_raw();
    let current = registry.get(&id).ok_or(Error::NotFound)?;
    if !Arc::ptr_eq(current, &unregistration.registered) {
        return Err(Error::Registered);
    }
    let state = current.state.lock();
    if state.status != RegistrationStatus::Removing || state.lease_count != 0 {
        return Err(Error::Busy);
    }
    drop(state);

    let registered = registry.remove(&id).unwrap();
    Ok(registered.device.clone())
}

/// Aborts a pending unregistration, re-publishing the device.
pub fn abort_unregister(unregistration: PendingBlockDeviceUnregistration) -> Result<(), Error> {
    let registry = DEVICE_REGISTRY.lock();
    let current = registry
        .get(&unregistration.id().to_raw())
        .ok_or(Error::NotFound)?;
    if !Arc::ptr_eq(current, &unregistration.registered) {
        return Err(Error::Registered);
    }
    let mut state = current.state.lock();
    if state.status != RegistrationStatus::Removing || state.lease_count != 0 {
        return Err(Error::Busy);
    }
    state.status = RegistrationStatus::Live;
    Ok(())
}

/// Unregisters an existing block device (convenience: begin + commit).
///
/// Returns `Error::Busy` if the device still has outstanding leases.
pub fn unregister(id: DeviceId) -> Result<Arc<dyn BlockDevice>, Error> {
    let unregistration = begin_unregister(id)?;
    commit_unregister(unregistration)
}

/// Collects all live (published) block devices.
pub fn collect_all() -> Vec<Arc<dyn BlockDevice>> {
    DEVICE_REGISTRY
        .lock()
        .values()
        .filter_map(|registered| {
            let state = registered.state.lock();
            (state.status == RegistrationStatus::Live).then(|| registered.device.clone())
        })
        .collect()
}

/// Looks up a live block device by its device ID.
pub fn lookup(id: DeviceId) -> Option<Arc<dyn BlockDevice>> {
    let registry = DEVICE_REGISTRY.lock();
    let registered = registry.get(&id.to_raw())?;
    let state = registered.state.lock();
    (state.status == RegistrationStatus::Live).then(|| registered.device.clone())
}

/// Looks up a block device by its kernel device name.
pub fn lookup_by_name(name: &str) -> Option<Arc<dyn BlockDevice>> {
    let registry = DEVICE_REGISTRY.lock();
    registry
        .values()
        .find(|registered| registered.device.name() == name)
        .map(|registered| registered.device.clone())
}

/// Acquires a lease on a live block device by its device ID.
///
/// Long-term users (e.g., DM targets, mounted filesystems) must hold a lease
/// for the entire duration in which they may issue I/O.
pub fn lookup_lease(id: DeviceId) -> Option<BlockDeviceLease> {
    let registry = DEVICE_REGISTRY.lock();
    let registered = registry.get(&id.to_raw())?;
    let mut state = registered.state.lock();
    if state.status != RegistrationStatus::Live {
        return None;
    }
    state.lease_count = state.lease_count.checked_add(1)?;
    drop(state);

    Some(BlockDeviceLease {
        device: registered.device.clone(),
        registered: Some(registered.clone()),
    })
}

static DEVICE_REGISTRY: Mutex<BTreeMap<u32, Arc<RegisteredBlockDevice>>> =
    Mutex::new(BTreeMap::new());

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    device_id::init();

    Ok(())
}

/// Scans registered whole-disk devices and updates their partitions.
pub fn scan_partitions() {
    let devices = collect_all();
    for device in devices {
        if device.is_partition() {
            continue;
        }

        let Some(partition_info) = partition::parse(&device) else {
            continue;
        };

        device.set_partitions(partition_info);
    }
}
