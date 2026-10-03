// SPDX-License-Identifier: GPL-2.0

//! INode abstractions for the VFS.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use bitflags::bitflags;
use spin::RwLock;

use crate::api::errno::Result;
use super::file::{FileOps, OpenFlags};

bitflags! {
    /// File types and permission bits mirroring standard POSIX modes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Mode: u32 {
        // File types
        const FIFO      = 0o010000;
        const CHAR      = 0o020000;
        const DIR       = 0o040000;
        const BLOCK     = 0o060000;
        const FILE      = 0o100000;
        const LINK      = 0o120000;
        const SOCKET    = 0o140000;

        // Special flags
        const SUID      = 0o004000;
        const SGID      = 0o002000;
        const STICKY    = 0o001000;

        // User permissions
        const RUSR      = 0o000400;
        const WUSR      = 0o000200;
        const XUSR      = 0o000100;

        // Group permissions
        const RGRP      = 0o000040;
        const WGRP      = 0o000020;
        const XGRP      = 0o000010;

        // Others permissions
        const ROTH      = 0o000004;
        const WOTH      = 0o000002;
        const XOTH      = 0o000001;

        // Standard aliases
        const READ      = 0o000444;
        const WRITE     = 0o000222;
        const EXEC      = 0o000111;
    }
}

/// Mask selecting the file-type bits of a mode.
pub const S_IFMT: u32 = 0o170000;

/// Metadata attributes for an inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct INodeAttr {
    pub size: usize,
    pub mode: Mode,
    pub nlink: usize,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
}

impl Default for INodeAttr {
    fn default() -> Self {
        Self {
            size: 0,
            mode: Mode::empty(),
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        }
    }
}

/// Operations that can be performed on a specific inode.
pub trait NodeOps: Send + Sync {
    /// Lookup a directory entry by name.
    fn lookup(&self, _name: &str) -> Result<Arc<INode>> {
        crate::return_errno!(ENOTDIR, "not a directory");
    }

    /// Create a new file or directory.
    fn create(&self, _name: &str, _mode: Mode) -> Result<Arc<INode>> {
        crate::return_errno!(ENOSYS, "create not implemented");
    }

    /// Remove a directory entry.
    fn unlink(&self, _name: &str) -> Result<()> {
        crate::return_errno!(ENOSYS, "unlink not implemented");
    }

    /// Create a hard link.
    fn link(&self, _name: &str, _other: Arc<INode>) -> Result<()> {
        crate::return_errno!(ENOSYS, "link not implemented");
    }

    /// Create a special node (device, FIFO, socket) named `name`.
    fn mknod(&self, _name: &str, _mode: Mode, _rdev: u64) -> Result<Arc<INode>> {
        crate::return_errno!(ENOSYS, "mknod not implemented");
    }

    /// Move the entry `old_name` of this directory to `new_name` inside `new_dir`.
    ///
    /// The VFS has already validated permissions, sticky bits and that a
    /// directory is not moved into its own subtree.
    fn rename(&self, _old_name: &str, _new_dir: &Arc<INode>, _new_name: &str) -> Result<()> {
        crate::return_errno!(ENOSYS, "rename not implemented");
    }

    /// Create a symbolic link `name` pointing at `target`.
    fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<INode>> {
        crate::return_errno!(ENOSYS, "symlink not implemented");
    }

    /// Read the target path of a symbolic link.
    fn readlink(&self) -> Result<String> {
        crate::return_errno!(EINVAL, "not a symbolic link");
    }

    /// Create a directory.
    fn mkdir(&self, _name: &str, _mode: Mode) -> Result<Arc<INode>> {
        crate::return_errno!(ENOSYS, "mkdir not implemented");
    }

    /// Remove a directory.
    fn rmdir(&self, _name: &str) -> Result<()> {
        crate::return_errno!(ENOSYS, "rmdir not implemented");
    }

    /// Opens the inode and creates file operations for an open file description.
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        crate::return_errno!(ENOSYS, "open not implemented");
    }

    /// Retrieves updated metadata attributes.
    fn getattr(&self) -> Result<INodeAttr> {
        crate::return_errno!(ENOSYS, "getattr not implemented");
    }

    /// Updates metadata attributes.
    fn setattr(&self, _attr: &INodeAttr) -> Result<()> {
        crate::return_errno!(ENOSYS, "setattr not implemented");
    }
}

/// Access / modification / status-change times, in seconds since boot.
///
/// Kept next to (not inside) [`INodeAttr`] so filesystem drivers that build an
/// `INodeAttr` literal are unaffected. Drivers with on-disk timestamps may
/// overwrite them through [`INode::set_times`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct INodeTimes {
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

/// Preferred I/O block size reported through `stat`.
pub const DEFAULT_BLKSIZE: usize = 4096;

fn now_secs() -> u64 {
    crate::sched::now() / crate::sched::TIMER_HZ
}

/// A standalone file system node (vnode).
///
/// Represents a file, directory, or special device in a generic way within the VFS.
pub struct INode {
    /// Inode number (unique identifier within the filesystem).
    pub id: usize,
    /// Node-specific operations.
    pub node_ops: Box<dyn NodeOps>,
    /// Consolidated metadata attributes protected by a single RwLock.
    pub attr: RwLock<INodeAttr>,
    /// Access / modification / change times.
    pub times: RwLock<INodeTimes>,
}

impl INode {
    /// Creates a new INode.
    pub fn new(ops: Box<dyn NodeOps>, mode: Mode, id: usize) -> Self {
        Self::new_with_attr(
            ops,
            id,
            INodeAttr {
                mode,
                ..Default::default()
            },
        )
    }

    /// Creates an INode with full initial attributes.
    pub fn new_with_attr(ops: Box<dyn NodeOps>, id: usize, attr: INodeAttr) -> Self {
        let now = now_secs();
        Self {
            id,
            node_ops: ops,
            attr: RwLock::new(attr),
            times: RwLock::new(INodeTimes {
                atime: now,
                mtime: now,
                ctime: now,
            }),
        }
    }

    /// Returns the timestamps of this inode.
    pub fn times(&self) -> INodeTimes {
        *self.times.read()
    }

    /// Overwrites the timestamps (for drivers with on-disk times).
    pub fn set_times(&self, times: INodeTimes) {
        *self.times.write() = times;
    }

    /// Records an access (`atime`).
    pub fn touch_atime(&self) {
        self.times.write().atime = now_secs();
    }

    /// Records a content modification (`mtime` and `ctime`).
    pub fn touch_mtime(&self) {
        let now = now_secs();
        let mut t = self.times.write();
        t.mtime = now;
        t.ctime = now;
    }

    /// Records a metadata change (`ctime`).
    pub fn touch_ctime(&self) {
        self.times.write().ctime = now_secs();
    }

    /// Preferred I/O block size for `stat`.
    pub fn blksize(&self) -> usize {
        DEFAULT_BLKSIZE
    }

    /// Number of 512-byte blocks allocated, for `stat`.
    pub fn blocks(&self) -> usize {
        self.size().div_ceil(512)
    }

    /// Returns the file-type bits (`S_IFMT`) of this inode's mode.
    fn file_type(&self) -> u32 {
        self.attr.read().mode.bits() & S_IFMT
    }

    /// Checks if the inode represents a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type() == Mode::DIR.bits()
    }

    /// Checks if the inode represents a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type() == Mode::FILE.bits()
    }

    /// Checks if the inode represents a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.file_type() == Mode::LINK.bits()
    }

    /// Returns the current size of the inode.
    pub fn size(&self) -> usize {
        self.attr.read().size
    }

    /// Sets the size of the inode.
    pub fn set_size(&self, size: usize) {
        self.attr.write().size = size;
    }
}
