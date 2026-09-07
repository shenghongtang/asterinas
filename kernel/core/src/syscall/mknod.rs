// SPDX-License-Identifier: MPL-2.0

use super::SyscallReturn;
use crate::{
    fs::{
        self,
        file::{InodeMode, InodeType, file_table::RawFileDesc},
        vfs::{
            inode::MknodType,
            path::{AT_FDCWD, EmptyPathStr, FsPath},
        },
    },
    prelude::*,
    syscall::constants::MAX_FILENAME_LEN,
};

pub(super) fn sys_mknodat(
    dirfd: RawFileDesc,
    path_addr: Vaddr,
    mode: u16,
    dev: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let path_name = ctx.user_space().read_cstring(path_addr, MAX_FILENAME_LEN)?;
    let fs_ref = ctx.thread_local.borrow_fs();
    let inode_mode = {
        let mask_mode = mode & !fs_ref.umask().get();
        InodeMode::from_bits_truncate(mask_mode)
    };
    let inode_type = InodeType::from_raw_mode(mode)?;
    debug!(
        "dirfd = {}, path = {:?}, inode_mode = {:?}, inode_type = {:?}, dev = {}",
        dirfd, path_name, inode_mode, inode_type, dev
    );

    let (dir_path, name) = {
        let path_name = path_name.to_string_lossy();
        let fs_path = FsPath::from_fd_at(dirfd, &path_name, EmptyPathStr::Reject)?;
        fs_ref
            .resolver()
            .read()
            .lookup_unresolved_no_follow(&fs_path)?
            .into_parent_and_filename()?
    };

    // Resolve the dev number for DM block device nodes.
    //
    // libdevmapper calls mknod("/dev/mapper/<name>", S_IFBLK, dev) but may
    // pass dev=0 (when udev is absent) or an old-format dev. Always try to
    // resolve the correct dev from the DM registry by device name first.
    let dev = if inode_type == InodeType::BlockDevice {
        resolve_dm_dev(&name).unwrap_or(dev as u64)
    } else {
        dev as u64
    };

    match inode_type {
        InodeType::File => {
            let _ = dir_path.new_child(&name, InodeType::File, inode_mode)?;
        }
        InodeType::CharDevice => {
            let _ = dir_path.mknod(&name, inode_mode, MknodType::CharDevice(dev))?;
        }
        InodeType::BlockDevice => {
            let _ = dir_path.mknod(&name, inode_mode, MknodType::BlockDevice(dev))?;
        }
        InodeType::NamedPipe => {
            let _ = dir_path.mknod(&name, inode_mode, MknodType::NamedPipe)?;
        }
        InodeType::Socket => {
            let _ = dir_path.new_child(&name, InodeType::Socket, inode_mode)?;
        }
        _ => return_errno_with_message!(Errno::EPERM, "unimplemented file types"),
    }
    fs::vfs::notify::on_create(&dir_path, || name);
    Ok(SyscallReturn::Return(0))
}

/// Resolves the dev_t (glibc-encoded u64) for a DM device by name.
///
/// Returns `None` if the name doesn't match any registered DM device.
fn resolve_dm_dev(name: &str) -> Option<u64> {
    let device = aster_dm::MappedDevice::lookup_by_name(name)?;
    Some(device.device_id().as_encoded_u64())
}

pub(super) fn sys_mknod(
    path_addr: Vaddr,
    mode: u16,
    dev: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    sys_mknodat(AT_FDCWD, path_addr, mode, dev, ctx)
}
