// SPDX-License-Identifier: GPL-2.0

//! Filesystem subsystem for SilicaOS.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;

pub mod devfs;
pub mod exfat;
pub mod ext2;
pub mod fd;
pub mod initramfs;
pub mod pipe;
pub mod poll;
pub mod registry;
pub mod vfs;

pub use fd::*;
pub use initramfs::read_file_from_initramfs;
pub use registry::*;
pub use vfs::{
    File, FileOps, FileSystem, INode, Mode, OpenFlags, PathNode, dcache::DEntry, lookup,
    mount::Mount, open, perms, root, set_root,
};

use crate::fs::initramfs::{InitramfsDirNodeOps, InitramfsFs};

/// Initializes the VFS root filesystem and mounts core device nodes.
pub fn init() {
    // 1. Initialize root filesystem from initramfs
    let initramfs_fs = Arc::new(InitramfsFs::new());
    let root_inode = initramfs_fs
        .root()
        .expect("failed to get initramfs root inode");
    let root_dentry = Arc::new(DEntry::new(String::new(), Some(root_inode), None));
    let root_mount = Arc::new(Mount::new(Arc::clone(&root_dentry), initramfs_fs));
    let root_node = PathNode {
        mount: root_mount,
        dentry: root_dentry,
    };
    vfs::set_root(root_node.clone());

    // 2. Ensure /dev mount directory exists in root
    if root_node.dentry.lookup_child("dev").is_none() {
        let dev_node_ops = Box::new(InitramfsDirNodeOps::new());
        let dev_inode = Arc::new(INode::new(
            dev_node_ops,
            Mode::DIR | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::ROTH,
            998,
        ));
        let dev_dentry = Arc::new(DEntry::new(
            "dev".into(),
            Some(dev_inode),
            Some(Arc::downgrade(&root_node.dentry)),
        ));
        root_node.dentry.add_child(dev_dentry);
    }

    // 3. Create /mnt mount directory if not present
    let mnt_node_ops = Box::new(InitramfsDirNodeOps::new());
    let mnt_inode = Arc::new(INode::new(
        mnt_node_ops,
        Mode::DIR | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::ROTH,
        999,
    ));
    let mnt_dentry = Arc::new(DEntry::new(
        "mnt".into(),
        Some(mnt_inode),
        Some(Arc::downgrade(&root_node.dentry)),
    ));
    root_node.dentry.add_child(mnt_dentry);

    ostd::info!("VFS initialized: mounted Initramfs at /, /dev and /mnt ready");
}
