// SPDX-License-Identifier: GPL-2.0

//! Mount point and PathNode abstractions for the VFS.

use alloc::sync::Arc;
use spin::RwLock;

use crate::errno::{Errno, Result};
use super::dcache::DEntry;
use super::fs::FileSystem;

/// Represents a mounted filesystem instance.
pub struct Mount {
    /// The mount point in the parent filesystem (if any).
    pub mount_point: RwLock<Option<PathNode>>,
    /// The root dentry of this mounted filesystem.
    pub root: Arc<DEntry>,
    /// The filesystem implementation driving this mount.
    pub fs: Arc<dyn FileSystem>,
}

impl Mount {
    /// Creates a new Mount instance for a filesystem root.
    pub fn new(root: Arc<DEntry>, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            mount_point: RwLock::new(None),
            root,
            fs,
        }
    }
}

/// Represents a specific location in the VFS, pairing a dentry with its mount instance.
#[derive(Clone)]
pub struct PathNode {
    /// The mount instance this node belongs to.
    pub mount: Arc<Mount>,
    /// The directory entry.
    pub dentry: Arc<DEntry>,
}

impl PathNode {
    /// Mounts another filesystem on top of this path node.
    pub fn mount(&self, new_mount: Arc<Mount>) -> Result<()> {
        self.dentry.mounts.write().push(new_mount.clone());
        *new_mount.mount_point.write() = Some(self.clone());
        Ok(())
    }
}

/// Global root mount node of the VFS namespace.
pub static VFS_ROOT: RwLock<Option<PathNode>> = RwLock::new(None);

/// Retrieves a clone of the global root PathNode.
pub fn root() -> Result<PathNode> {
    VFS_ROOT.read().clone().ok_or(Errno::ENOENT)
}

/// Sets the global root PathNode.
pub fn set_root(node: PathNode) {
    *VFS_ROOT.write() = Some(node);
}
