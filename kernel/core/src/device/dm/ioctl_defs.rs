// SPDX-License-Identifier: MPL-2.0

//! Linux-compatible device-mapper ioctl definitions.
//!
//! These structures and constants match the interface defined in
//! `<linux/dm-ioctl.h>`, allowing LVM2 and other userspace tools to
//! communicate with the Asterinas device mapper without modification.

#![allow(dead_code)]

/// The DM ioctl "magic" type number (same as Linux's `DM_IOCTL`).
pub const DM_IOCTL_TYPE: u8 = 0xfd;

/// The DM ioctl version returned to userspace: `[major, minor, patch]`.
///
/// Linux reports 4.48.0; the minor must be high enough that modern
/// libdevmapper (4.3x/4.4x, minor <= 48) passes the version check.
pub const DM_VERSION: [u32; 3] = [4, 48, 0];

// ---------------------------------------------------------------------------
// Ioctl command numbers (the `nr` byte extracted from the full command).
// ---------------------------------------------------------------------------

pub const DM_VERSION_NR: u8 = 0x00;
pub const DM_REMOVE_ALL_NR: u8 = 0x01;
pub const DM_LIST_DEVICES_NR: u8 = 0x02;
pub const DM_DEV_CREATE_NR: u8 = 0x03;
pub const DM_DEV_REMOVE_NR: u8 = 0x04;
pub const DM_DEV_RENAME_NR: u8 = 0x05;
pub const DM_DEV_SUSPEND_NR: u8 = 0x06;
pub const DM_DEV_STATUS_NR: u8 = 0x07;
pub const DM_DEV_WAIT_NR: u8 = 0x08;
pub const DM_TABLE_LOAD_NR: u8 = 0x09;
pub const DM_TABLE_CLEAR_NR: u8 = 0x0a;
pub const DM_TABLE_DEPS_NR: u8 = 0x0b;
pub const DM_TABLE_STATUS_NR: u8 = 0x0c;
pub const DM_LIST_VERSIONS_NR: u8 = 0x0d;
pub const DM_TARGET_MSG_NR: u8 = 0x0e;
pub const DM_DEV_SET_GEOMETRY_NR: u8 = 0x0f;
pub const DM_GET_TARGET_VERSION_NR: u8 = 0x10;

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Read-only device.
pub const DM_READONLY_FLAG: u32 = 1 << 0;
/// If set in `DM_DEV_SUSPEND`, suspend the device; if clear, resume it.
pub const DM_SUSPEND_FLAG: u32 = 1 << 1;
/// Userspace/libdevmapper-only flag: if clear in a `DM_DEV_STATUS` response,
/// the requested device does not exist. Not defined in the Linux kernel header,
/// but LVM2 checks this bit, so the kernel must report it correctly.
pub const DM_EXISTS_FLAG: u32 = 1 << 2;
/// Preserve the device node after removing the device.
pub const DM_PERSISTENT_DEV_FLAG: u32 = 1 << 3;
/// In `DM_TABLE_STATUS`, return the table (parameters) rather than runtime status.
pub const DM_STATUS_TABLE_FLAG: u32 = 1 << 4;
/// Out: an active table is present.
pub const DM_ACTIVE_PRESENT_FLAG: u32 = 1 << 5;
/// Out: an inactive table is present.
pub const DM_INACTIVE_PRESENT_FLAG: u32 = 1 << 6;
/// Out: the data buffer was too small; results are truncated.
pub const DM_BUFFER_FULL_FLAG: u32 = 1 << 8;
/// Ignored in modern kernels; kept for compatibility with older userspace.
pub const DM_SKIP_BDGET_FLAG: u32 = 1 << 9;
/// Avoid attempting to freeze any filesystem when suspending.
pub const DM_SKIP_LOCKFS_FLAG: u32 = 1 << 10;
/// In `DM_DEV_SUSPEND`, do not flush the device.
pub const DM_NOFLUSH_FLAG: u32 = 1 << 11;
/// In `DM_TABLE_STATUS` or `DM_DEV_STATUS`, query the inactive table instead of the active one.
pub const DM_QUERY_INACTIVE_TABLE_FLAG: u32 = 1 << 12;
/// Out: a uevent was generated for which the caller may need to wait.
pub const DM_UEVENT_GENERATED_FLAG: u32 = 1 << 13;
/// In `DM_DEV_RENAME`, rename the UUID instead of the name.
pub const DM_UUID_FLAG: u32 = 1 << 14;
/// Wipe all buffers after use when sending or requesting sensitive data.
pub const DM_SECURE_DATA_FLAG: u32 = 1 << 15;
/// Out: a message generated output data.
pub const DM_DATA_OUT_FLAG: u32 = 1 << 16;
/// In/Out: schedule device for removal when it gets closed if still busy.
pub const DM_DEFERRED_REMOVE: u32 = 1 << 17;
/// Out: the device is suspended internally.
pub const DM_INTERNAL_SUSPEND_FLAG: u32 = 1 << 18;
/// Return raw table information that would be measured by IMA.
pub const DM_IMA_MEASUREMENT_FLAG: u32 = 1 << 19;

/// The `dm_ioctl` header structure (312 bytes).
///
/// This is the fixed-size header that precedes variable-length data in every
/// DM ioctl call. It matches Linux's `struct dm_ioctl` exactly.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct DmIoctl {
    /// DM ioctl version `[major, minor, patch]` - input/output.
    pub version: [u32; 3],
    /// Total size of the ioctl buffer (header + data) - input.
    pub data_size: u32,
    /// Offset from the start of this struct to the data area - input.
    pub data_start: u32,
    /// Number of targets - input for `DM_TABLE_LOAD`, output for others.
    pub target_count: u32,
    /// Open count - output.
    pub open_count: u32,
    /// Flags - input/output.
    pub flags: u32,
    /// Event number - input/output.
    pub event_nr: u32,
    /// Reserved padding.
    pub padding1: u32,
    /// Device major:minor (encoded) - input/output.
    pub dev: u64,
    /// Device name - input/output (null-terminated).
    pub name: [u8; 128],
    /// Device UUID - input/output (null-terminated).
    pub uuid: [u8; 129],
    /// Padding to bring the struct to 312 bytes.
    pub data: [u8; 7],
}

impl DmIoctl {
    /// Extracts the device name as a `&str`.
    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(128);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    /// Sets the device name, null-terminating and zero-filling the rest.
    pub fn set_name(&mut self, name: &str) {
        self.name = [0u8; 128];
        let bytes = name.as_bytes();
        let len = bytes.len().min(127);
        self.name[..len].copy_from_slice(&bytes[..len]);
    }

    /// Extracts the device UUID as a `&str`.
    pub fn uuid_str(&self) -> &str {
        let end = self.uuid.iter().position(|&b| b == 0).unwrap_or(129);
        core::str::from_utf8(&self.uuid[..end]).unwrap_or("")
    }

    /// Sets the device UUID, null-terminating and zero-filling the rest.
    pub fn set_uuid(&mut self, uuid: &str) {
        self.uuid = [0u8; 129];
        let bytes = uuid.as_bytes();
        // The UUID field is 129 bytes; reserve one byte for the null
        // terminator, so at most 128 bytes of UUID text.
        let len = bytes.len().min(128);
        self.uuid[..len].copy_from_slice(&bytes[..len]);
    }
}

/// A target specification entry (40 bytes).
///
/// Used in `DM_TABLE_LOAD` (input) and `DM_TABLE_STATUS` (output). The
/// `target_type` field is followed by a null-terminated parameters string
/// that is not part of this struct.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct DmTargetSpec {
    /// Logical start sector on the mapped device.
    pub sector_start: u64,
    /// Length in sectors.
    pub length: u64,
    /// Status - output.
    pub status: u32,
    /// Offset from the start of `DmIoctl` to the next `DmTargetSpec`
    /// (0 = last entry).
    pub next: u32,
    /// Target type name (null-terminated, e.g. `"linear"`).
    pub target_type: [u8; 16],
}

impl DmTargetSpec {
    /// Extracts the target type as a `&str`.
    pub fn target_type_str(&self) -> &str {
        let end = self.target_type.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.target_type[..end]).unwrap_or("")
    }

    /// Sets the target type, null-terminating and zero-filling the rest.
    pub fn set_target_type(&mut self, ttype: &str) {
        self.target_type = [0u8; 16];
        let bytes = ttype.as_bytes();
        let len = bytes.len().min(15);
        self.target_type[..len].copy_from_slice(&bytes[..len]);
    }
}

/// Aligns a value up to the next multiple of 8.
pub fn align8(len: usize) -> usize {
    (len + 7) & !7
}
