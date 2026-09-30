// SPDX-License-Identifier: GPL-2.0

//! Path resolution, lookup, and high-level VFS open.

use alloc::sync::Arc;
use bitflags::bitflags;

use crate::errno::{Errno, Result};
use super::dcache::DEntry;
use super::file::{File, OpenFlags};
use super::mount::PathNode;

bitflags! {
    /// Flags for path lookup operations.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct LookupFlags: u32 {
        /// Do not follow terminal symlinks.
        const NO_FOLLOW = 1 << 0;
        /// Target must be a directory.
        const DIRECTORY = 1 << 1;
    }
}

/// Resolves a path string starting from a base directory or root.
pub fn lookup(
    root: PathNode,
    start: PathNode,
    path: &str,
    _flags: LookupFlags,
) -> Result<PathNode> {
    let mut current = if path.starts_with('/') {
        root.clone()
    } else {
        start
    };

    let mut components = path.split('/').filter(|s| !s.is_empty()).peekable();

    while let Some(comp) = components.next() {
        match comp {
            "." => {
                // Stay on current directory
            }
            ".." => {
                // Check if at the root of a mounted filesystem
                if Arc::ptr_eq(&current.dentry, &current.mount.root) {
                    let mount_point = current.mount.mount_point.read().clone();
                    if let Some(parent_node) = mount_point {
                        current = parent_node;
                    }
                }

                // Move up the dentry tree
                let parent_weak = current.dentry.parent.clone();
                if let Some(parent_weak) = parent_weak {
                    if let Some(parent_dentry) = parent_weak.upgrade() {
                        current.dentry = parent_dentry;
                    }
                }
            }
            name => {
                // Fast path: cached child dentry
                let resolved_dentry = if let Some(entry) = current.dentry.lookup_child(name) {
                    entry
                } else {
                    // Slow path: query inode NodeOps
                    let inode = current.dentry.get_inode().ok_or(Errno::ENOENT)?;
                    let child_inode = inode.node_ops.lookup(name)?;
                    let new_dentry = Arc::new(DEntry::new(
                        name.into(),
                        Some(child_inode),
                        Some(Arc::downgrade(&current.dentry)),
                    ));
                    current.dentry.add_child(new_dentry.clone());
                    new_dentry
                };

                // Check if the resolved dentry is a mount point
                let mount_opt = resolved_dentry.mounts.read().last().cloned();
                if let Some(last_mount) = mount_opt {
                    current = PathNode {
                        mount: last_mount.clone(),
                        dentry: last_mount.root.clone(),
                    };
                } else {
                    current.dentry = resolved_dentry;
                }
            }
        }
    }

    Ok(current)
}

/// Opens a file by path starting from the global VFS root.
pub fn open(path: &str, flags: OpenFlags) -> Result<Arc<File>> {
    let root = super::mount::root()?;
    let path_node = lookup(root.clone(), root, path, LookupFlags::empty())?;
    let inode = path_node.dentry.get_inode().ok_or(Errno::ENOENT)?;

    if flags.contains(OpenFlags::DIRECTORY) && !inode.is_dir() {
        return Err(Errno::ENOTDIR);
    }

    let ops = inode.node_ops.open(flags)?;
    let seekable = inode.is_file();

    let mut file = File::new(ops, inode, flags, seekable);
    file.abs_path = Some(path.as_bytes().to_vec());

    Ok(Arc::new(file))
}
