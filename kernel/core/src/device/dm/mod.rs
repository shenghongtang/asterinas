// SPDX-License-Identifier: MPL-2.0

//! Device mapper control device (`/dev/mapper/control`).
//!
//! This module registers a character device that accepts Linux-compatible
//! `DM_*` ioctl commands, allowing userspace tools (such as LVM2) to create,
//! configure, suspend, and remove device-mapper virtual block devices.

mod control;
mod ioctl_defs;
mod parser;

pub(super) fn init_in_first_kthread() {
    // Registering /dev/mapper/control requires `aster_dm` to be initialized
    // first (it allocates the DM block major). Rely on the component init
    // order instead of calling `ensure_initialized()` here.
    assert!(
        aster_dm::is_initialized(),
        "aster_dm must be initialized before registering /dev/mapper/control"
    );
    control::init();
}

/// Creates `/dev/dm-<minor>` and `/dev/mapper/<name>` devtmpfs nodes for all
/// DM devices that were created from `dm_mod.create=` kernel command-line
/// entries during component init.
///
/// The block device registry already creates `/dev/<name>` for each registered
/// block device, but the Linux-compatible `/dev/dm-<minor>` and
/// `/dev/mapper/<name>` symlinks/nodes are only created by the ioctl path
/// (`DM_DEV_CREATE`). This function ensures that boot-created devices also get
/// those nodes so that userspace tools (e.g., `dmsetup`) can find them.
pub(super) fn init_in_first_process() {
    for (name, _uuid, device_id) in aster_dm::MappedDevice::list_devices() {
        let minor = device_id.minor().get();
        let dev = device_id.as_encoded_u64();
        control::create_dev_node(&name, dev, minor);
        ostd::info!(
            "created devtmpfs nodes for boot dm device '{}' ({}:{})",
            name,
            device_id.major().get(),
            minor
        );
    }
}
