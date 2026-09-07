// SPDX-License-Identifier: MPL-2.0

//! `/proc/devices` — lists all registered character and block device majors.
//!
//! Reference: <https://man7.org/linux/man-pages/man5/proc_devices.5.html>

use aster_util::printer::VmPrinter;

use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcFile, ProcFileOps},
        vfs::inode::Inode,
    },
    prelude::*,
};

/// Represents the inode at `/proc/devices`.
pub struct DevicesFileOps;

impl DevicesFileOps {
    pub fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        ProcFile::new(Self, parent, mkmod!(a+r))
    }
}

impl ProcFileOps for DevicesFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);

        writeln!(printer, "Character devices:")?;
        writeln!(printer)?;
        writeln!(printer, "Block devices:")?;
        for (major, name) in aster_block::major_devices() {
            writeln!(printer, "{major:3} {name}")?;
        }

        Ok(printer.bytes_written())
    }
}
