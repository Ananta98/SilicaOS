// SPDX-License-Identifier: GPL-2.0

//! File abstractions for the VFS.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::bitflags;
use spin::Mutex;

use crate::api::errno::{Errno, Result};
use super::inode::INode;

bitflags! {
    /// File open flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct OpenFlags: u32 {
        const READ      = 1 << 0;
        const WRITE     = 1 << 1;
        const APPEND    = 1 << 2;
        const DIRECTORY = 1 << 3;
        const CLOEXEC   = 1 << 4;
        const CREATE    = 1 << 5;
        const TRUNC     = 1 << 6;
        const EXCL      = 1 << 7;
        const NONBLOCK  = 1 << 8;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekAnchor {
    /// Seek relative to the start of the file.
    Start(u64),
    /// Seek relative to the current cursor position.
    Current(i64),
    /// Seek relative to the end of the file.
    End(i64),
}

/// A generic file operations trait that represents the capabilities of an open file or device.
pub trait FileOps: Send + Sync {
    /// Read data from the file at `offset` into the provided buffer.
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write data from the buffer into the file at `offset`.
    fn write(&self, offset: u64, buf: &[u8]) -> Result<usize>;

    /// Seek to a different position in the file.
    fn seek(&self, curr_offset: u64, anchor: SeekAnchor) -> Result<u64> {
        let _ = (curr_offset, anchor);
        crate::return_errno!(ENOSYS, "seek not implemented");
    }

    /// Read directory entries.
    fn readdir(&self) -> Result<Vec<u8>> {
        crate::return_errno!(ENOTDIR, "not a directory");
    }

    /// Perform a device-specific control command.
    fn ioctl(&self, cmd: u32, arg: usize) -> Result<usize> {
        let _ = (cmd, arg);
        crate::return_errno!(ENOTTY, "inappropriate ioctl for device");
    }

    /// Close the file.
    fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// The kernel representation of an open file description.
pub struct File {
    /// Absolute path resolved at open time, if available.
    pub abs_path: Option<Vec<u8>>,
    /// Operations that can be performed on this file.
    pub ops: Box<dyn FileOps>,
    /// The opened inode.
    pub inode: Arc<INode>,
    /// File open flags.
    pub flags: Mutex<OpenFlags>,
    /// Current cursor position within the file.
    pub offset: Mutex<u64>,
    /// Whether this file supports cursor offsets and seeking (regular files vs streams/pipes).
    pub seekable: bool,
}

impl File {
    /// Constructs a new open file description.
    pub fn new(ops: Box<dyn FileOps>, inode: Arc<INode>, flags: OpenFlags, seekable: bool) -> Self {
        Self {
            abs_path: None,
            ops,
            inode,
            flags: Mutex::new(flags),
            offset: Mutex::new(0),
            seekable,
        }
    }

    /// Reads data from the file, advancing the cursor if seekable.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if !self.flags.lock().contains(OpenFlags::READ) {
            return Err(Errno::EBADF);
        }

        let mut offset_guard = self.offset.lock();
        let curr_offset = *offset_guard;
        let nread = self.ops.read(curr_offset, buf)?;

        if self.seekable {
            *offset_guard = curr_offset.saturating_add(nread as u64);
        }

        Ok(nread)
    }

    /// Writes data to the file, advancing the cursor if seekable and updating inode size.
    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        let flags = *self.flags.lock();
        if !flags.contains(OpenFlags::WRITE) {
            return Err(Errno::EBADF);
        }

        let mut offset_guard = self.offset.lock();
        if flags.contains(OpenFlags::APPEND) && self.seekable {
            *offset_guard = self.inode.size() as u64;
        }

        let curr_offset = *offset_guard;
        let nwritten = self.ops.write(curr_offset, buf)?;

        if self.seekable {
            let new_offset = curr_offset.saturating_add(nwritten as u64);
            *offset_guard = new_offset;
            if new_offset as usize > self.inode.size() {
                self.inode.set_size(new_offset as usize);
            }
        }

        Ok(nwritten)
    }

    /// Repositions the read/write cursor.
    pub fn seek(&self, anchor: SeekAnchor) -> Result<u64> {
        if !self.seekable {
            return Err(Errno::ESPIPE);
        }

        let mut offset_guard = self.offset.lock();
        let new_offset = self.ops.seek(*offset_guard, anchor)?;
        *offset_guard = new_offset;
        Ok(new_offset)
    }

    /// Performs an ioctl on the underlying file operations.
    pub fn ioctl(&self, cmd: u32, arg: usize) -> Result<usize> {
        self.ops.ioctl(cmd, arg)
    }

    /// Closes the underlying file operations.
    pub fn close(&self) -> Result<()> {
        self.ops.close()
    }
}
