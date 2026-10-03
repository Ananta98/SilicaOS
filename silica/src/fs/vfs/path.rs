// SPDX-License-Identifier: GPL-2.0

//! Path resolution, lookup, and high-level VFS open.

use alloc::sync::Arc;
use bitflags::bitflags;

use crate::api::errno::{Errno, Result};
use super::dcache::DEntry;
use super::file::{File, OpenFlags};
use super::mount::PathNode;
use super::perms::{check_permission, current_cred, AccessFlags};

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

/// Maximum number of symlinks followed during a single lookup.
pub const MAX_SYMLINK_DEPTH: usize = 40;

/// Resolves a path string starting from a base directory or root.
pub fn lookup(
    root: PathNode,
    start: PathNode,
    path: &str,
    flags: LookupFlags,
) -> Result<PathNode> {
    let mut depth = 0;
    let node = lookup_inner(&root, start, path, flags, &mut depth)?;
    if flags.contains(LookupFlags::DIRECTORY) {
        let inode = node.dentry.get_inode().ok_or(Errno::ENOENT)?;
        if !inode.is_dir() {
            return Err(Errno::ENOTDIR);
        }
    }
    Ok(node)
}

fn lookup_inner(
    root: &PathNode,
    start: PathNode,
    path: &str,
    flags: LookupFlags,
    depth: &mut usize,
) -> Result<PathNode> {
    let mut current = if path.starts_with('/') {
        root.clone()
    } else {
        start
    };

    let mut components = path.split('/').filter(|s| !s.is_empty()).peekable();

    while let Some(comp) = components.next() {
        let is_last = components.peek().is_none();
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
                // Searching a directory requires execute permission on it.
                let dir_inode = current.dentry.get_inode().ok_or(Errno::ENOENT)?;
                if !dir_inode.is_dir() {
                    return Err(Errno::ENOTDIR);
                }
                let dir_attr = *dir_inode.attr.read();
                check_permission(&dir_attr, &current_cred(), AccessFlags::EXEC)?;

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

                // Follow symlinks (except a terminal one under NO_FOLLOW)
                if let Some(inode) = resolved_dentry.get_inode() {
                    if inode.is_symlink() && !(is_last && flags.contains(LookupFlags::NO_FOLLOW)) {
                        *depth += 1;
                        if *depth > MAX_SYMLINK_DEPTH {
                            return Err(Errno::ELOOP);
                        }
                        let target = inode.node_ops.readlink()?;
                        if target.is_empty() {
                            return Err(Errno::ENOENT);
                        }
                        // Relative targets resolve against the symlink's parent directory.
                        current = lookup_inner(root, current, &target, LookupFlags::empty(), depth)?;
                        continue;
                    }
                }

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

    // Permission check against the inode's mode bits.
    let mut access = AccessFlags::empty();
    if flags.contains(OpenFlags::READ) {
        access |= AccessFlags::READ;
    }
    if flags.contains(OpenFlags::WRITE) || flags.contains(OpenFlags::TRUNC) {
        access |= AccessFlags::WRITE;
    }
    let attr = *inode.attr.read();
    check_permission(&attr, &current_cred(), access)?;

    let ops = inode.node_ops.open(flags)?;
    let seekable = inode.is_file();

    // O_TRUNC: only meaningful for regular files opened for writing.
    if flags.contains(OpenFlags::TRUNC) && flags.contains(OpenFlags::WRITE) && inode.is_file() {
        let mut attr = *inode.attr.read();
        attr.size = 0;
        inode.node_ops.setattr(&attr)?;
        inode.set_size(0);
    }

    let mut file = File::new(ops, inode, flags, seekable);
    file.abs_path = Some(path.as_bytes().to_vec());

    Ok(Arc::new(file))
}
