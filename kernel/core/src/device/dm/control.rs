// SPDX-License-Identifier: MPL-2.0

//! The `/dev/mapper/control` character device.
//!
//! This device accepts Linux-compatible `DM_*` ioctl commands, allowing
//! userspace tools (such as LVM2) to create, configure, suspend, and remove
//! device-mapper virtual block devices.
//!
//! # Supported ioctl commands
//!
//! | Command           | Description                          |
//! |-------------------|--------------------------------------|
//! | `DM_VERSION`      | Return the DM ioctl version          |
//! | `DM_LIST_DEVICES` | List all mapped devices              |
//! | `DM_DEV_CREATE`   | Create a new mapped device           |
//! | `DM_DEV_REMOVE`   | Remove a mapped device               |
//! | `DM_DEV_SUSPEND`  | Suspend or resume a device           |
//! | `DM_DEV_STATUS`   | Get device status                    |
//! | `DM_TABLE_LOAD`   | Load a target table into a device    |
//! | `DM_TABLE_CLEAR`  | Clear the target table of a device   |
//! | `DM_TABLE_DEPS`   | List underlying devices of a table   |
//! | `DM_TABLE_STATUS` | Get the current table of a device    |
//! | `DM_LIST_VERSIONS`| List supported target types          |
//! | `DM_DEV_SET_GEOMETRY` | Set device geometry (no-op)     |

use alloc::format;
use core::ops::{Deref, DerefMut};

use aster_dm::{DmTable, MappedDevice, TableError, get_target_version, list_target_versions};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::mm::VmIo;

use super::ioctl_defs::*;
use crate::{
    context::current_userspace,
    device::{Device, DeviceType, registry::char},
    events::IoEvents,
    fs::{
        devtmpfs::{DevtmpfsNode, DevtmpfsNodeMeta, create_node, delete_node},
        file::{PerOpenFileOps, StatusFlags},
        vfs::{inode::FileOps, path::Path},
    },
    prelude::*,
    process::signal::{PollHandle, Pollable},
    util::ioctl::RawIoctl,
};

/// The major ID for the DM control device.
///
/// The DM control device uses the misc major (10), shared with other misc
/// devices like hwrng. The misc module acquires this major during boot.
const DM_CTRL_MAJOR: MajorId = MajorId::new(10);

/// The minor ID for the DM control device.
///
/// This matches the Linux convention where `/dev/mapper/control` is registered
/// as a misc device with minor 236 (`MAPPER_CTRL_MINOR`).
/// Reference: <https://elixir.bootlin.com/linux/v6.13/source/include/uapi/linux/misc.h#L63>
const DM_CTRL_MINOR: MinorId = MinorId::new(236);

// ---------------------------------------------------------------------------
// Device and file handle
// ---------------------------------------------------------------------------

/// The `/dev/mapper/control` character device.
#[derive(Debug)]
struct DmControlDevice {
    id: DeviceId,
}

impl DmControlDevice {
    fn new() -> Arc<Self> {
        let id = DeviceId::new(DM_CTRL_MAJOR, DM_CTRL_MINOR);
        Arc::new(Self { id })
    }
}

impl Device for DmControlDevice {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        DevtmpfsNodeMeta::new("mapper/control").ok()
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        Ok(Box::new(DmControlFile))
    }
}

/// A file handle for `/dev/mapper/control`.
struct DmControlFile;

impl Pollable for DmControlFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        mask & (IoEvents::IN | IoEvents::OUT)
    }
}

impl FileOps for DmControlFile {
    fn read_at(
        &self,
        _offset: usize,
        _writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        return_errno_with_message!(Errno::EPERM, "read is not supported on dm control");
    }

    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        return_errno_with_message!(Errno::EPERM, "write is not supported on dm control");
    }
}

impl PerOpenFileOps for DmControlFile {
    fn check_seekable(&self) -> Result<()> {
        Ok(())
    }

    fn is_offset_aware(&self) -> bool {
        false
    }

    fn ioctl(&self, _path: &Path, raw_ioctl: RawIoctl) -> Result<i32> {
        handle_ioctl(raw_ioctl)
    }
}

// ---------------------------------------------------------------------------
// Devtmpfs node management
// ---------------------------------------------------------------------------

/// Creates devtmpfs nodes for a mapped device.
///
/// Two block device nodes are created, both with the correct dev:
/// - `/dev/dm-<minor>` - the kernel-standard DM device node
/// - `/dev/mapper/<name>` - the userspace-friendly name
///
/// Without this, libdevmapper creates `/dev/mapper/<name>` with dev=0
/// because there is no udev daemon to set the correct dev.
pub(super) fn create_dev_node(name: &str, dev_id: u64, minor: u32) {
    let Some(id) = DeviceId::from_encoded_u64(dev_id) else {
        ostd::warn!("invalid device id {} for dm device '{}'", dev_id, name);
        return;
    };

    // Create /dev/dm-<minor> block device node.
    let dm_path = format!("dm-{}", minor);
    create_node_quietly(id, &dm_path, name);

    // Create /dev/mapper/<name> as a block device node with the correct
    // dev. libdevmapper may later call mknod() to (re)create this node;
    // the ramfs mknod handler replaces existing device nodes and resolves
    // the correct dev from the DM registry, so the node stays consistent.
    let mapper_path = format!("mapper/{}", name);
    create_node_quietly(id, &mapper_path, name);
}

/// Creates a block devtmpfs node, replacing any stale node left by a
/// previous (possibly failed) create.
fn create_node_quietly(id: DeviceId, path: &str, name: &str) {
    let Ok(meta) = DevtmpfsNodeMeta::new(path.to_string()) else {
        ostd::warn!("invalid devtmpfs path '{}' for dm device '{}'", path, name);
        return;
    };
    // Remove existing node first (e.g., from a previous create that failed).
    let _ = delete_node(DevtmpfsNode::new(DeviceType::Block, id, meta.clone()));
    if let Err(e) = create_node(DevtmpfsNode::new(DeviceType::Block, id, meta)) {
        ostd::warn!(
            "failed to create /dev/{} for dm device '{}': {:?}",
            path,
            name,
            e
        );
    }
}

/// Removes devtmpfs nodes for a mapped device.
fn remove_dev_node(name: &str, id: DeviceId) {
    let minor = id.minor().get();
    for path in [format!("mapper/{}", name), format!("dm-{}", minor)] {
        let Ok(meta) = DevtmpfsNodeMeta::new(path.clone()) else {
            continue;
        };
        if let Err(e) = delete_node(DevtmpfsNode::new(DeviceType::Block, id, meta)) {
            ostd::debug!("failed to remove /dev/{} for dm device: {:?}", path, e);
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Registers the `/dev/mapper/control` device.
///
/// The DM control device uses the misc major (10) with minor 236, matching the
/// Linux convention so that userspace tools (e.g., LVM2/dmsetup) can find it at
/// the expected device node without manual `mknod`.
pub(super) fn init() {
    let device = DmControlDevice::new();
    char::register(device).expect("failed to register dm control device");
    ostd::info!(
        "device mapper control device registered at /dev/mapper/control ({}:{})",
        DM_CTRL_MAJOR.get(),
        DM_CTRL_MINOR.get()
    );
}

// ---------------------------------------------------------------------------
// Version check
// ---------------------------------------------------------------------------

/// Validates the DM ioctl version supplied by userspace.
///
/// The kernel supports any userspace version whose major number matches and
/// whose minor number is not greater than the kernel's minor. The patch level
/// is ignored for compatibility checks; the response header always carries
/// the kernel's patch level.
fn check_ioctl_version(header: &DmIoctl) -> Result<()> {
    let [req_major, req_minor, _] = header.version;
    let [kernel_major, kernel_minor, _] = DM_VERSION;

    if req_major != kernel_major {
        return_errno_with_message!(Errno::EINVAL, "incompatible DM ioctl major version");
    }

    if req_minor > kernel_minor {
        return_errno_with_message!(Errno::EINVAL, "DM ioctl minor version too new");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Ioctl dispatch
// ---------------------------------------------------------------------------

/// The main ioctl handler: reads the `DmIoctl` header, dispatches to the
/// appropriate command handler, and writes the updated header back.
fn handle_ioctl(raw_ioctl: RawIoctl) -> Result<i32> {
    let cmd = raw_ioctl.cmd();
    let nr = (cmd & 0xFF) as u8;
    let type_ = ((cmd >> 8) & 0xFF) as u8;

    if type_ != DM_IOCTL_TYPE {
        return_errno_with_message!(Errno::ENOTTY, "not a DM ioctl");
    }

    // Read the header from userspace.
    let mut header: DmIoctl = current_userspace!()
        .read_val(raw_ioctl.arg())
        .map_err(|_| Error::with_message(Errno::EFAULT, "failed to read dm_ioctl header"))?;

    // Always set the version in the response.
    header.version = DM_VERSION;

    // Validate the userspace-supplied version for all commands except
    // DM_VERSION, which is used to probe compatibility.
    if nr != DM_VERSION_NR {
        check_ioctl_version(&header)?;
    }

    // Validate the data area description for commands that read from or
    // write to it. DM_VERSION carries no data area.
    if nr != DM_VERSION_NR {
        check_data_area(&header)?;
    }

    let result = match nr {
        DM_VERSION_NR => Ok(()),
        DM_REMOVE_ALL_NR => handle_remove_all(&mut header),
        DM_LIST_DEVICES_NR => handle_list_devices(&mut header, raw_ioctl.arg()),
        DM_DEV_CREATE_NR => handle_dev_create(&mut header),
        DM_DEV_REMOVE_NR => handle_dev_remove(&mut header),
        DM_DEV_RENAME_NR => handle_dev_rename(&mut header, raw_ioctl.arg()),
        DM_DEV_SUSPEND_NR => handle_dev_suspend(&mut header),
        DM_DEV_STATUS_NR => handle_dev_status(&mut header),
        DM_DEV_WAIT_NR => handle_dev_wait(&mut header),
        DM_TARGET_MSG_NR => handle_target_msg(&mut header),
        DM_TABLE_LOAD_NR => handle_table_load(&mut header, raw_ioctl.arg()),
        DM_TABLE_CLEAR_NR => handle_table_clear(&mut header),
        DM_TABLE_DEPS_NR => handle_table_deps(&mut header, raw_ioctl.arg()),
        DM_TABLE_STATUS_NR => handle_table_status(&mut header, raw_ioctl.arg()),
        DM_LIST_VERSIONS_NR => handle_list_versions(&mut header, raw_ioctl.arg()),
        DM_DEV_SET_GEOMETRY_NR => handle_dev_set_geometry(&mut header),
        DM_GET_TARGET_VERSION_NR => handle_get_target_version(&mut header, raw_ioctl.arg()),
        _ => {
            return_errno_with_message!(Errno::ENOTTY, "unsupported DM ioctl command");
        }
    };

    if result.is_err() {
        ostd::debug!(
            "dm ioctl nr=0x{:x} name='{}' uuid='{}' dev={} failed: {:?}",
            nr,
            header.name_str(),
            header.uuid_str(),
            header.dev,
            result
        );
    }

    // Always write the header back (even on error, to report version info).
    let _ = current_userspace!().write_val(raw_ioctl.arg(), &header);

    result.map(|_| 0)
}

/// Returns the data_start offset, defaulting to `size_of::<DmIoctl>()` if 0.
fn data_start(header: &DmIoctl) -> usize {
    if header.data_start > 0 {
        header.data_start as usize
    } else {
        size_of::<DmIoctl>()
    }
}

/// Validates the userspace-supplied data area description.
///
/// Linux checks `data_start >= sizeof(struct dm_ioctl)` and
/// `data_start <= data_size` in `copy_params`. Without this, a malicious
/// `data_start > data_size` makes `data_size - data_start` underflow in
/// handlers such as `handle_table_deps` (panic in debug, OOM in release),
/// and a `data_start` smaller than the header lets the data area overlap
/// the ioctl header.
fn check_data_area(header: &DmIoctl) -> Result<()> {
    let ds = data_start(header);
    if ds < size_of::<DmIoctl>() || ds > header.data_size as usize {
        return_errno_with_message!(Errno::EINVAL, "invalid data area description");
    }
    Ok(())
}

/// Builds DM ioctl replies that serialize variable-length entries into the
/// data area (e.g. `DM_LIST_DEVICES`, `DM_TABLE_STATUS`,
/// `DM_LIST_VERSIONS`).
///
/// Entries are padded to 8-byte boundaries and linked by a `next` offset
/// stored at a fixed position within each entry; the last entry has
/// `next == 0`. This helper centralizes the padding and the bounds check
/// that every list-style handler would otherwise duplicate.
struct DataAreaWriter {
    buf: Vec<u8>,
}

impl DataAreaWriter {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Returns the offset in the buffer at which the current entry starts.
    fn begin_entry(&mut self) -> usize {
        self.buf.len()
    }

    /// Pads the entry that began at `start` to an 8-byte boundary.
    fn end_entry(&mut self, start: usize) {
        let len = self.buf.len() - start;
        self.buf.resize(start + align8(len), 0);
    }

    /// Writes the buffered entries to the ioctl's data area.
    ///
    /// Returns `ENOSPC` if the entries do not fit within `data_size`.
    fn commit(self, header: &DmIoctl, arg: usize) -> Result<()> {
        let ds = data_start(header);
        if ds + self.buf.len() > header.data_size as usize {
            return_errno_with_message!(Errno::ENOSPC, "buffer too small for ioctl data");
        }
        current_userspace!().write_bytes(arg + ds, &self.buf)?;
        Ok(())
    }
}

impl Deref for DataAreaWriter {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.buf
    }
}

impl DerefMut for DataAreaWriter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buf
    }
}

/// Looks up a mapped device by name, UUID, or encoded dev field.
///
/// LVM2 commonly sets the `name` field to the device's UUID (e.g.
/// `LVM-<vg_uuid><lv_uuid>`) when issuing `DM_DEV_STATUS`, `DM_DEV_SUSPEND`,
/// and `DM_DEV_REMOVE` ioctls, rather than using the actual device name. So
/// when the name lookup fails, we retry the same string as a UUID. This
/// mirrors the Linux kernel's `find_device`, which checks both the name and
/// UUID hash tables. Without this fallback, `lvcreate` fails with "No such
/// file or directory" during LV activation because LVM2 cannot find the
/// device it just created.
fn lookup_device(header: &DmIoctl) -> Option<Arc<MappedDevice>> {
    let name = header.name_str();
    if !name.is_empty() {
        if let Some(dev) = MappedDevice::lookup_by_name(name) {
            return Some(dev);
        }
        // LVM2 puts the UUID in the name field for lookups; retry as UUID.
        if let Some(dev) = MappedDevice::lookup_by_uuid(name) {
            return Some(dev);
        }
    }
    let uuid = header.uuid_str();
    if !uuid.is_empty()
        && let Some(dev) = MappedDevice::lookup_by_uuid(uuid)
    {
        return Some(dev);
    }
    if header.dev != 0 {
        return MappedDevice::lookup_by_dev(header.dev);
    }
    None
}

// ---------------------------------------------------------------------------
// Individual command handlers
// ---------------------------------------------------------------------------

/// Updates the ioctl header flags to reflect the actual device state.
///
/// Mirrors Linux's `__dev_status()`: sets/clears `DM_SUSPEND_FLAG` and
/// `DM_ACTIVE_PRESENT_FLAG` based on the device's current state. LVM2 checks
/// these flags in the response of `DM_DEV_CREATE`, `DM_TABLE_LOAD`, etc. to
/// decide whether a resume is needed.
fn update_dev_status(header: &mut DmIoctl, device: &MappedDevice) {
    header.dev = device.device_id().as_encoded_u64();

    // The device exists, so report DM_EXISTS_FLAG. LVM2/libdevmapper checks
    // this bit in responses to DM_DEV_STATUS and other ioctls to decide
    // whether the requested device is present.
    header.flags |= DM_EXISTS_FLAG;

    // Acquire each table lock once for a consistent snapshot and to avoid
    // the cost of multiple independent lock/unlock pairs.
    let (active_table, inactive_table) = device.table_states();
    header.target_count = active_table
        .as_ref()
        .map(|t| t.num_targets() as u32)
        .unwrap_or(0);

    if device.is_suspended() {
        header.flags |= DM_SUSPEND_FLAG;
    } else {
        header.flags &= !DM_SUSPEND_FLAG;
    }

    if active_table.is_some() {
        header.flags |= DM_ACTIVE_PRESENT_FLAG;
    } else {
        header.flags &= !DM_ACTIVE_PRESENT_FLAG;
    }

    if inactive_table.is_some() {
        header.flags |= DM_INACTIVE_PRESENT_FLAG;
    } else {
        header.flags &= !DM_INACTIVE_PRESENT_FLAG;
    }

    if device.is_readonly() {
        header.flags |= DM_READONLY_FLAG;
    } else {
        header.flags &= !DM_READONLY_FLAG;
    }

    header.open_count = device.open_count();
}

/// DM_REMOVE_ALL - removes all mapped devices (best-effort).
///
/// Mirrors Linux's `DM_REMOVE_ALL`: busy devices (those with outstanding
/// leases, e.g. a mounted filesystem) are skipped. Non-busy devices are
/// removed and their devtmpfs nodes cleaned up. The ioctl always succeeds
/// even if some devices could not be removed.
fn handle_remove_all(header: &mut DmIoctl) -> Result<()> {
    // Snapshot the device list - remove_by_name mutates the registry.
    let devices = MappedDevice::list_devices();
    let mut removed = 0u32;
    for (name, id) in devices {
        match MappedDevice::remove_by_name(name.as_ref()) {
            Ok(_device) => {
                remove_dev_node(name.as_ref(), id);
                removed += 1;
                ostd::debug!("remove_all: removed dm device '{}' (id={:?})", name, id,);
            }
            Err(aster_dm::DmError::DeviceBusy) => {
                ostd::debug!(
                    "remove_all: skipping busy dm device '{}' (id={:?})",
                    name,
                    id,
                );
            }
            Err(e) => {
                ostd::warn!(
                    "remove_all: failed to remove dm device '{}' (id={:?}): {:?}",
                    name,
                    id,
                    e,
                );
            }
        }
    }
    ostd::info!("remove_all: removed {} dm device(s)", removed);
    // No specific device to report; clear the header fields.
    header.dev = 0;
    header.target_count = 0;
    header.flags &= !(DM_SUSPEND_FLAG | DM_ACTIVE_PRESENT_FLAG | DM_INACTIVE_PRESENT_FLAG);
    Ok(())
}

/// DM_LIST_DEVICES - writes `dm_name_list` entries to the data area.
fn handle_list_devices(header: &mut DmIoctl, arg: usize) -> Result<()> {
    let devices = MappedDevice::list_devices();

    // Compute entry sizes: dev(8) + next(4) + name + null, padded to 8.
    let entry_sizes: Vec<usize> = devices
        .iter()
        .map(|(name, _)| align8(12 + name.len() + 1))
        .collect();

    let total: usize = entry_sizes.iter().sum();
    let mut writer = DataAreaWriter::with_capacity(total);
    for (i, (name, id)) in devices.iter().enumerate() {
        let entry_start = writer.begin_entry();
        let next = if i + 1 < devices.len() {
            entry_sizes[i] as u32
        } else {
            0
        };
        writer.extend_from_slice(&id.as_encoded_u64().to_le_bytes()); // dev
        writer.extend_from_slice(&next.to_le_bytes()); // next
        writer.extend_from_slice(name.as_bytes()); // name
        writer.push(0); // null terminator
        writer.end_entry(entry_start);
    }

    writer.commit(header, arg)?;
    header.target_count = devices.len() as u32;
    Ok(())
}

/// DM_DEV_CREATE - creates a new mapped device by name.
///
/// If the caller supplies a UUID (e.g. LVM2's `LVM-<vg><lv>` UUID), it is
/// stored on the device so that subsequent ioctls can look it up by UUID.
fn handle_dev_create(header: &mut DmIoctl) -> Result<()> {
    let name: Arc<str> = Arc::from(header.name_str());
    if name.is_empty() {
        return_errno_with_message!(Errno::EINVAL, "device name is empty");
    }
    let uuid = header.uuid_str().to_string();
    let device = match MappedDevice::create_empty(name.clone()) {
        Ok(device) => device,
        Err(aster_dm::DmError::NotInitialized) => {
            return Err(Error::with_message(
                Errno::ENODEV,
                "device mapper not initialized",
            ));
        }
        Err(aster_dm::DmError::AlreadyRegistered) => {
            // LVM2's lvextend/lvreduce calls DM_DEV_CREATE on an existing
            // device. Some LVM2 versions do not fall back to reload on
            // EEXIST - they simply abort the entire operation.
            //
            // Workaround: reset and reuse the existing device. Resetting
            // clears both active and inactive tables and leaves the device
            // unsuspended. Because no active table remains after reset,
            // LVM2's subsequent DM_TABLE_LOAD writes the new table directly
            // into the active slot, so I/O works even if the resume ioctl
            // is skipped by libdevmapper's suspended-device counter.
            if let Some(existing) = MappedDevice::lookup_by_name(name.as_ref()) {
                // Set the UUID before resetting the device. If setting the UUID
                // fails, the device retains its old tables and suspended state;
                // resetting first would leave it in a half-initialized state.
                if !uuid.is_empty() {
                    existing.set_uuid(&uuid).map_err(|e| match e {
                        aster_dm::DmError::UuidExists => {
                            Error::with_message(Errno::EEXIST, "uuid already in use")
                        }
                        _ => Error::with_message(Errno::EINVAL, "failed to set device uuid"),
                    })?;
                }
                existing.reset();
                // Recreate devtmpfs nodes (they were removed by the prior
                // DM_DEV_REMOVE so that the VFS inode cache is invalidated
                // for /dev/dm-<minor> and /dev/mapper/<name>).
                let dev_id = existing.device_id();
                let dev = dev_id.as_encoded_u64();
                let minor = dev_id.minor().get();
                create_dev_node(&name, dev, minor);
                // Honor the read-only flag supplied at creation time.
                existing.set_readonly(header.flags & DM_READONLY_FLAG != 0);
                update_dev_status(header, &existing);
                return Ok(());
            }
            return Err(Error::with_message(Errno::EEXIST, "device already exists"));
        }
        Err(aster_dm::DmError::MinorExhausted) => {
            return Err(Error::with_message(
                Errno::ENOSPC,
                "minor numbers exhausted",
            ));
        }
        Err(_) => {
            return Err(Error::with_message(
                Errno::EINVAL,
                "failed to create device",
            ));
        }
    };
    if !uuid.is_empty() {
        device.set_uuid(&uuid).map_err(|e| match e {
            aster_dm::DmError::UuidExists => {
                Error::with_message(Errno::EEXIST, "uuid already in use")
            }
            _ => Error::with_message(Errno::EINVAL, "failed to set device uuid"),
        })?;
    }
    let dev_id = device.device_id();
    let dev = dev_id.as_encoded_u64();
    let minor = dev_id.minor().get();

    // Create devtmpfs nodes so userspace can open the device without manual mknod.
    create_dev_node(&name, dev, minor);

    // Honor the read-only flag supplied at creation time.
    device.set_readonly(header.flags & DM_READONLY_FLAG != 0);

    // Update header flags (device has no table yet).
    update_dev_status(header, &device);
    Ok(())
}

/// DM_DEV_REMOVE - removes a mapped device by name or UUID.
fn handle_dev_remove(header: &mut DmIoctl) -> Result<()> {
    // Use the shared lookup so UUID-based removal (used by LVM2) also works.
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;
    let name = device.name().to_string();

    // IMPORTANT: We do NOT call remove_by_name() (which would unregister the
    // device from the block layer and free its minor).  Instead, we only reset
    // the device state (clear tables, unsuspend) and clear its UUID.
    //
    // Rationale: VFS inodes for /dev/<vg>/<lv> nodes (created by LVM2 via
    // mknod, not managed by our devtmpfs) may cache the Arc<MappedDevice>.
    // If we unregister the old device and create a fresh one in the next
    // DM_DEV_CREATE, the VFS inode still points to the OLD device.  This
    // causes:
    //   1. BLKGETSIZE64 returns the old device's table size (wrong capacity)
    //   2. Writes go through the old device's table (potentially wrong mapping)
    //
    // By keeping the device registered, the next DM_DEV_CREATE takes the
    // AlreadyRegistered path (lookup_by_name → found → reset → set_uuid),
    // which reuses the SAME device Arc.  The VFS inode then points to the
    // same device, which now has the new table loaded by DM_TABLE_LOAD.
    device.reset();
    // Clear the UUID so that UUID-based lookups (e.g., DM_DEV_STATUS for
    // -real/-cow suffixes) correctly return ENXIO after removal.
    let _ = device.set_uuid("");
    // Remove devtmpfs nodes (will be recreated by the next DM_DEV_CREATE).
    remove_dev_node(&name, device.device_id());
    Ok(())
}

/// DM_DEV_RENAME - renames a mapped device.
///
/// The existing device is identified by the header's name/UUID/dev fields.
/// The new name is read from the data area (a null-terminated string at
/// `data_start`). If `DM_UUID_FLAG` is set, the ioctl would rename the UUID
/// instead - this is not supported and returns `EOPNOTSUPP`.
///
/// On success, the device's internal name, the `DM_DEVICES` registry key,
/// and the `/dev/mapper/<name>` devtmpfs node are all updated. The
/// `/dev/dm-<minor>` node is keyed by minor number and is unchanged.
fn handle_dev_rename(header: &mut DmIoctl, arg: usize) -> Result<()> {
    if (header.flags & DM_UUID_FLAG) != 0 {
        return_errno_with_message!(Errno::EOPNOTSUPP, "renaming the DM UUID is not supported");
    }

    // Identify the existing device by name/UUID/dev in the header.
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;
    let old_name = device.name();

    // Read the new name from the data area.
    let ds = data_start(header);
    let new_name = read_cstring_from_user(arg + ds, 128)?;
    if new_name.is_empty() {
        return_errno_with_message!(Errno::EINVAL, "new device name is empty");
    }

    // No-op if the name is unchanged.
    if new_name == old_name.as_ref() {
        update_dev_status(header, &device);
        return Ok(());
    }

    // Rename in the registry and on the device. This atomically updates
    // both the BTreeMap key and the device's internal name field.
    MappedDevice::rename_by_name(old_name.as_ref(), &new_name).map_err(|e| match e {
        aster_dm::DmError::NotFound => Error::with_message(Errno::ENXIO, "device not found"),
        aster_dm::DmError::AlreadyRegistered => {
            Error::with_message(Errno::EEXIST, "new name already in use")
        }
        _ => Error::with_message(Errno::EINVAL, "failed to rename device"),
    })?;

    // Update devtmpfs nodes: remove the old /dev/mapper/<old_name> node
    // and create /dev/mapper/<new_name> with the same dev id.
    let dev_id = device.device_id().as_encoded_u64();
    let minor = device.device_id().minor().get();
    remove_dev_node(old_name.as_ref(), device.device_id());
    create_dev_node(&new_name, dev_id, minor);

    ostd::info!(
        "renamed dm device '{}' -> '{}' (id={:?})",
        old_name,
        new_name,
        device.device_id(),
    );

    // Echo back the new name and updated status.
    header.set_name(&new_name);
    update_dev_status(header, &device);
    Ok(())
}

/// DM_DEV_SUSPEND - suspends (flag set) or resumes (flag clear) a device.
///
/// If `DM_NOFLUSH_FLAG` is set, the suspend does not wait for in-flight
/// I/O to complete. This mirrors Linux's behavior for `dmsetup suspend
/// --noflush`.
fn handle_dev_suspend(header: &mut DmIoctl) -> Result<()> {
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;
    let suspend = (header.flags & DM_SUSPEND_FLAG) != 0;
    if suspend && (header.flags & DM_NOFLUSH_FLAG) != 0 {
        device.set_suspended_no_flush(true);
    } else {
        device.set_suspended(suspend);
    }
    update_dev_status(header, &device);
    Ok(())
}

/// DM_DEV_STATUS - returns the status of a device.
fn handle_dev_status(header: &mut DmIoctl) -> Result<()> {
    let device = match lookup_device(header) {
        Some(dev) => dev,
        None => {
            // Linux returns -ENXIO for DM_DEV_STATUS on a non-existent
            // device. LVM2/libdevmapper treats this as "device does not
            // exist" (e.g. when probing snapshot component devices like
            // `-real` and `-cow`). Returning success here can confuse LVM2
            // into thinking an empty/inactive device exists and prevent
            // deactivation.
            return Err(Error::with_message(Errno::ENXIO, "device not found"));
        }
    };
    // Echo back the name and UUID so LVM2 can correlate the device.
    header.set_name(device.name().as_ref());
    header.set_uuid(device.uuid().as_ref());
    update_dev_status(header, &device);
    Ok(())
}

/// DM_DEV_WAIT - waits for the next event on a device.
///
/// If the device's current event number is not greater than the caller's
/// `header.event_nr`, blocks until `load_table`, `clear_table`, or
/// `set_suspended` bumps the counter. The returned header reflects the
/// latest device status and the new event number.
fn handle_dev_wait(header: &mut DmIoctl) -> Result<()> {
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;

    let last = header.event_nr;
    if device.event_nr() <= last {
        device.wait_event(last);
    }

    update_dev_status(header, &device);
    header.event_nr = device.event_nr();
    Ok(())
}

/// DM_TARGET_MSG - passes a message to a target.
///
/// Target messages are not supported; return `EOPNOTSUPP` so userspace
/// tools can detect the lack of support gracefully.
fn handle_target_msg(_header: &mut DmIoctl) -> Result<()> {
    Err(Error::with_message(
        Errno::EOPNOTSUPP,
        "target messages not supported",
    ))
}

/// DM_TABLE_LOAD - reads target specs from the data area and loads a table.
fn handle_table_load(header: &mut DmIoctl, arg: usize) -> Result<()> {
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;

    let ds = data_start(header);
    let base = arg + ds;
    let mut table = DmTable::new();
    let mut offset = 0usize;

    // `offset` is relative to the start of the data area, so the usable
    // size is `data_size - ds` (check_data_area guarantees ds <= data_size).
    let data_size = header.data_size as usize - ds;

    for _ in 0..header.target_count {
        // Guard against reading the 40-byte DmTargetSpec beyond the data area.
        if offset + size_of::<DmTargetSpec>() > data_size {
            return Err(Error::with_message(
                Errno::EINVAL,
                "target spec beyond data area",
            ));
        }

        // Read the 40-byte DmTargetSpec.
        let spec: DmTargetSpec = current_userspace!()
            .read_val(base + offset)
            .map_err(|_| Error::with_message(Errno::EFAULT, "failed to read target spec"))?;

        // Guard against reading params beyond the data area; cap the read at
        // the remaining bytes after the spec.
        let params_max = core::cmp::min(
            256,
            data_size.saturating_sub(offset + size_of::<DmTargetSpec>()),
        );
        if params_max == 0 {
            return Err(Error::with_message(
                Errno::EINVAL,
                "params area beyond data area",
            ));
        }

        // Read the params string (starts right after the spec).
        let params = read_cstring_from_user(base + offset + 40, params_max)?;

        let target_type = spec.target_type_str();
        let target = super::parser::parse_target(target_type, &params, spec.length)?;
        table
            .add_target(spec.sector_start, spec.length, target)
            .map_err(|err| match err {
                TableError::ZeroLength => {
                    Error::with_message(Errno::EINVAL, "target length must be non-zero")
                }
                TableError::NotContiguous => {
                    Error::with_message(Errno::EINVAL, "targets must be contiguous")
                }
                _ => Error::with_message(Errno::EINVAL, "invalid table"),
            })?;

        if spec.next == 0 {
            break;
        }
        let next = spec.next as usize;
        if next < size_of::<DmTargetSpec>() {
            return Err(Error::with_message(
                Errno::EINVAL,
                "target spec next offset too small",
            ));
        }
        offset = offset.checked_add(next).ok_or_else(|| {
            Error::with_message(Errno::EINVAL, "target spec next offset overflow")
        })?;
        if offset > data_size {
            return Err(Error::with_message(
                Errno::EINVAL,
                "target spec next offset beyond data area",
            ));
        }
    }

    device
        .load_table(table)
        .map_err(|_| Error::with_message(Errno::EINVAL, "failed to load table"))?;

    // Honor the read-only flag supplied with the table load.
    device.set_readonly(header.flags & DM_READONLY_FLAG != 0);

    update_dev_status(header, &device);
    Ok(())
}

/// DM_TABLE_CLEAR - clears the loaded table of a device.
///
/// LVM2 calls this during deactivation (`lvchange -an`): the device is
/// suspended first, then the table is cleared, and finally the device is
/// removed. Without this ioctl, LVM2 cannot properly deactivate LVs.
fn handle_table_clear(header: &mut DmIoctl) -> Result<()> {
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;
    device.clear_table();
    update_dev_status(header, &device);
    Ok(())
}

/// DM_TABLE_DEPS - writes the `dm_target_deps` structure to the data area.
///
/// Returns the list of unique underlying block device IDs that the table
/// depends on. The output layout is:
/// ```text
/// [dm_target_deps header (8 bytes: count u32 + padding u32)]
/// [dev array (count * 8 bytes: u64 each)]
/// ```
/// The `target_count` field in the `dm_ioctl` header is set to 1 (one deps
/// structure). If the provided buffer is too small, `count` is still set to
/// the actual number of dependencies so the caller can retry with a larger
/// buffer.
fn handle_table_deps(header: &mut DmIoctl, arg: usize) -> Result<()> {
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;

    // Fill in name, uuid, and status fields so libdevmapper can correlate
    // the device when building its dependency tree. Without the UUID,
    // LVM2's _uuid_prefix_matches check fails and deactivation is skipped.
    header.set_name(device.name().as_ref());
    header.set_uuid(device.uuid().as_ref());
    update_dev_status(header, &device);

    // Linux returns success with count=0 when no table is loaded.
    let table = match device.table() {
        Some(t) => t,
        None => {
            let ds = data_start(header);
            let buf = [0u8; 8]; // count=0 + padding=0
            current_userspace!().write_bytes(arg + ds, &buf)?;
            header.target_count = 0;
            return Ok(());
        }
    };

    let deps = table.deps();
    let count = deps.len() as u32;

    let ds = data_start(header);
    // dm_target_deps header: count(4) + padding(4) = 8 bytes
    // dev array: count * 8 bytes
    let needed = 8 + deps.len() * 8;

    // Always write the count + padding so the caller knows the needed size
    // even if the dev array doesn't fit.
    let mut buf = Vec::with_capacity(needed.min(header.data_size as usize - ds));
    buf.extend_from_slice(&count.to_le_bytes()); // count
    buf.extend_from_slice(&0u32.to_le_bytes()); // padding

    if ds + needed <= header.data_size as usize {
        // Buffer is large enough - append the dev array.
        for dev_id in &deps {
            buf.extend_from_slice(&dev_id.as_encoded_u64().to_le_bytes());
        }
        header.target_count = 1;
    } else {
        // Buffer too small - set data_size to the needed size so the caller
        // can retry. The count has already been written so the caller knows
        // how many devices to expect.
        header.data_size = (ds + needed) as u32;
    }

    current_userspace!().write_bytes(arg + ds, &buf)?;
    Ok(())
}

/// DM_TABLE_STATUS - writes target specs to the data area.
///
/// If `DM_QUERY_INACTIVE_TABLE_FLAG` is set in the ioctl flags, the inactive
/// table (loaded by `DM_TABLE_LOAD` but not yet swapped to active) is returned
/// instead of the active table. If `DM_STATUS_TABLE_FLAG` is set, the full
/// table parameters are returned; otherwise only the target type and runtime
/// status (currently empty for all targets) are returned. This matches Linux's
/// behavior for `dmsetup table` and `dmsetup status`.
fn handle_table_status(header: &mut DmIoctl, arg: usize) -> Result<()> {
    let device = lookup_device(header)
        .ok_or_else(|| Error::with_message(Errno::ENXIO, "device not found"))?;

    // Fill in name, uuid, and status fields for consistency with Linux.
    header.set_name(device.name().as_ref());
    header.set_uuid(device.uuid().as_ref());
    update_dev_status(header, &device);

    let table = if header.flags & DM_QUERY_INACTIVE_TABLE_FLAG != 0 {
        device.inactive_table()
    } else {
        device.table()
    };

    // Linux returns success with target_count=0 when no table is loaded.
    let table = match table {
        Some(t) => t,
        None => {
            header.target_count = 0;
            return Ok(());
        }
    };

    let infos = table.target_infos();
    let return_params = header.flags & DM_STATUS_TABLE_FLAG != 0;

    // Compute entry sizes: spec(40) + (params + null if requested), padded to 8.
    let entry_sizes: Vec<usize> = infos
        .iter()
        .map(|info| {
            let params_len = if return_params {
                info.params.len() + 1
            } else {
                0
            };
            align8(40 + params_len)
        })
        .collect();

    let total: usize = entry_sizes.iter().sum();
    let mut writer = DataAreaWriter::with_capacity(total);
    for (i, info) in infos.iter().enumerate() {
        let entry_start = writer.begin_entry();
        let next = if i + 1 < infos.len() {
            entry_sizes[i] as u32
        } else {
            0
        };

        // DmTargetSpec fields (40 bytes total).
        writer.extend_from_slice(&info.sector_start.to_le_bytes()); // sector_start (8)
        writer.extend_from_slice(&info.length.to_le_bytes()); // length (8)
        writer.extend_from_slice(&0u32.to_le_bytes()); // status (4)
        writer.extend_from_slice(&next.to_le_bytes()); // next (4)
        // target_type (16 bytes, null-terminated).
        let mut tt = [0u8; 16];
        let tt_bytes = info.target_type.as_bytes();
        let tt_len = tt_bytes.len().min(15);
        tt[..tt_len].copy_from_slice(&tt_bytes[..tt_len]);
        writer.extend_from_slice(&tt);

        // Params string (null-terminated) only when returning the table.
        if return_params {
            writer.extend_from_slice(info.params.as_bytes());
            writer.push(0);
        }

        writer.end_entry(entry_start);
    }

    writer.commit(header, arg)?;
    header.target_count = infos.len() as u32;
    Ok(())
}

/// DM_LIST_VERSIONS - writes `dm_target_versions` entries to the data area.
///
/// Layout (matches Linux `struct dm_target_versions`):
/// ```text
/// next(4) + version[3](12) + name + null, padded to 8 bytes.
/// ```
fn handle_list_versions(header: &mut DmIoctl, arg: usize) -> Result<()> {
    // Supported target types and their versions (major, minor, patch).
    //
    // `striped` is included because LVM2 uses the `striped` target for all
    // logical volumes, even single-stripe (linear) ones. LVM2 probes for the
    // `striped` target via DM_LIST_VERSIONS before attempting to create an LV;
    // without it, `lvcreate` fails with "Required device-mapper target(s) not
    // detected in your kernel". The `striped` target supports both
    // single-stripe (equivalent to `linear`) and multi-stripe (RAID0) configs.
    let targets = list_target_versions();

    // Compute entry sizes: next(4) + version[3](12) + name + null, padded to 8.
    let entry_sizes: Vec<usize> = targets
        .iter()
        .map(|(name, _)| align8(4 + 12 + name.len() + 1))
        .collect();

    let total: usize = entry_sizes.iter().sum();
    let mut writer = DataAreaWriter::with_capacity(total);
    for (i, (name, version)) in targets.iter().enumerate() {
        let entry_start = writer.begin_entry();
        let next = if i + 1 < targets.len() {
            entry_sizes[i] as u32
        } else {
            0
        };

        writer.extend_from_slice(&next.to_le_bytes()); // next (4)
        for &v in version {
            writer.extend_from_slice(&v.to_le_bytes()); // version[3] (12)
        }
        writer.extend_from_slice(name.as_bytes()); // name
        writer.push(0); // null terminator

        writer.end_entry(entry_start);
    }

    writer.commit(header, arg)?;
    header.target_count = targets.len() as u32;
    Ok(())
}

/// DM_GET_TARGET_VERSION - returns the version of a single target type.
///
/// The target name is read from the header's `name` field. The response
/// writes a single `dm_target_versions` entry (same layout as
/// `DM_LIST_VERSIONS`) to the data area. Returns `EINVAL` if the target
/// type is unknown.
fn handle_get_target_version(header: &mut DmIoctl, arg: usize) -> Result<()> {
    let target_name = header.name_str();
    if target_name.is_empty() {
        return_errno_with_message!(Errno::EINVAL, "target name is empty");
    }

    let version = get_target_version(target_name)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "unknown target type"))?;

    // Single entry: next(4) + version[3](12) + name + null, padded to 8.
    let entry_size = align8(4 + 12 + target_name.len() + 1);
    let mut writer = DataAreaWriter::with_capacity(entry_size);
    writer.extend_from_slice(&0u32.to_le_bytes()); // next = 0 (single entry)
    for &v in &version {
        writer.extend_from_slice(&v.to_le_bytes()); // version[3]
    }
    writer.extend_from_slice(target_name.as_bytes()); // name
    writer.push(0); // null terminator
    writer.end_entry(0);

    writer.commit(header, arg)?;
    header.target_count = 1;
    Ok(())
}

/// DM_DEV_SET_GEOMETRY - sets device geometry information.
///
/// In Linux, this stores cylinders/heads/sectors data for compatibility with
/// legacy tools. Asterinas does not use geometry information, so this is a
/// no-op that simply returns success. Without this handler, tools like
/// `mkfs.ext2` print "Unable to get device geometry" warnings and LVM2 may
/// fail certain operations that probe geometry.
fn handle_dev_set_geometry(_header: &mut DmIoctl) -> Result<()> {
    // Accept the ioctl silently. The geometry data in the payload is ignored.
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reads a null-terminated string from userspace (up to `max_len` bytes).
fn read_cstring_from_user(addr: usize, max_len: usize) -> Result<String> {
    let mut buf = vec![0u8; max_len];
    current_userspace!()
        .read_bytes(addr, &mut buf)
        .map_err(|_| Error::with_message(Errno::EFAULT, "failed to read params string"))?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(max_len);
    String::from_utf8(buf[..end].to_vec())
        .map_err(|_| Error::with_message(Errno::EINVAL, "invalid UTF-8 in params"))
}
