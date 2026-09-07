// SPDX-License-Identifier: MPL-2.0

//! Global device registry for the device mapper subsystem.
//!
//! The [`Registry`] maintains three consistent indices under one lock:
//! - `by_name`: name -> device (primary).
//! - `by_uuid`: UUID -> device (secondary).
//! - `by_dev`: device ID -> device (secondary).
//!
//! Keeping all indices under a single `RwLock` allows O(log n) UUID/device-ID
//! lookups while avoiding nested per-device locks during scans.

use alloc::{collections::BTreeMap, sync::Arc};

use device_id::DeviceId;

use crate::MappedDevice;

/// Global registry of mapped devices.
pub struct Registry {
    /// Primary index: name -> device.
    pub by_name: BTreeMap<Arc<str>, Arc<MappedDevice>>,
    /// Secondary index: UUID -> device.
    pub by_uuid: BTreeMap<Arc<str>, Arc<MappedDevice>>,
    /// Secondary index: device ID -> device.
    pub by_dev: BTreeMap<DeviceId, Arc<MappedDevice>>,
}

impl Registry {
    /// Creates an empty registry.
    pub const fn new() -> Self {
        Self {
            by_name: BTreeMap::new(),
            by_uuid: BTreeMap::new(),
            by_dev: BTreeMap::new(),
        }
    }

    /// Inserts a device into all indices.
    pub fn insert(&mut self, name: Arc<str>, device: Arc<MappedDevice>) {
        let id = device.id;
        if let Some(uuid) = Self::device_uuid(&device) {
            self.by_uuid.insert(uuid, device.clone());
        }
        self.by_dev.insert(id, device.clone());
        self.by_name.insert(name, device);
    }

    /// Removes a device by name from all indices.
    pub fn remove(&mut self, name: &str) -> Option<Arc<MappedDevice>> {
        let device = self.by_name.remove(name)?;
        let id = device.id;
        self.by_dev.remove(&id);
        if let Some(uuid) = Self::device_uuid(&device) {
            self.by_uuid.remove(&uuid);
        }
        Some(device)
    }

    /// Returns the non-empty UUID of a device, if any.
    pub fn device_uuid(device: &MappedDevice) -> Option<Arc<str>> {
        let uuid = device.uuid.lock().clone();
        if uuid.is_empty() { None } else { Some(uuid) }
    }
}
