// SPDX-License-Identifier: GPL-2.0

//! INode abstractions for the VFS.

use alloc::boxed::Box;
use alloc::sync::Arc;
use bitflags::bitflags;
use spin::RwLock;

use crate::errno::Result;
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
}

impl INode {
    /// Creates a new INode.
    pub fn new(ops: Box<dyn NodeOps>, mode: Mode, id: usize) -> Self {
        Self {
            id,
            node_ops: ops,
            attr: RwLock::new(INodeAttr {
                mode,
                ..Default::default()
            }),
        }
    }

    /// Creates an INode with full initial attributes.
    pub fn new_with_attr(ops: Box<dyn NodeOps>, id: usize, attr: INodeAttr) -> Self {
        Self {
            id,
            node_ops: ops,
            attr: RwLock::new(attr),
        }
    }

    /// Checks if the inode represents a directory.
    pub fn is_dir(&self) -> bool {
        self.attr.read().mode.contains(Mode::DIR)
    }

    /// Checks if the inode represents a regular file.
    pub fn is_file(&self) -> bool {
        self.attr.read().mode.contains(Mode::FILE)
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
