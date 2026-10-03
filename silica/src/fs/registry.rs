// SPDX-License-Identifier: GPL-2.0

//! Filesystem Type Registry and Device Mounting.
//!
//! Provides registration for filesystem drivers (e.g. `ext2`, `exfat`) loaded
//! via kernel modules and helpers to mount block devices into the VFS namespace.

use alloc::{
    collections::btree_map::BTreeMap,
    format,
    string::String,
    sync::Arc,
    vec::Vec,
};
use spin::Mutex;

use crate::{
    drivers::block::{get_block_device, list_block_devices, BlockDevice},
    api::errno::{Errno, Result},
    fs::vfs::{
        dcache::DEntry,
        lookup,
        mount::{root, Mount, PathNode},
        FileSystem, LookupFlags,
    },
};

/// Trait implemented by filesystem drivers that can be instantiated over a block device.
pub trait FileSystemType: Send + Sync {
    /// Human-readable filesystem type identifier (e.g. "ext2", "exfat").
    fn name(&self) -> &'static str;

    /// Attempts to mount the filesystem using the provided block device.
    fn mount(&self, dev: Arc<dyn BlockDevice>) -> Result<Arc<dyn FileSystem>>;
}

/// Global registry of available filesystem drivers.
static FS_REGISTRY: Mutex<BTreeMap<&'static str, Arc<dyn FileSystemType>>> =
    Mutex::new(BTreeMap::new());

/// Registers a filesystem type handler.
pub fn register_filesystem_type(fs_type: Arc<dyn FileSystemType>) {
    let name = fs_type.name();
    ostd::info!("VFS: Registered filesystem driver \"{}\"", name);
    let mut reg = FS_REGISTRY.lock();
    reg.insert(name, fs_type);
}

/// Retrieves a registered filesystem type by name.
pub fn get_filesystem_type(name: &str) -> Option<Arc<dyn FileSystemType>> {
    let reg = FS_REGISTRY.lock();
    reg.get(name).cloned()
}

/// Returns a list of all registered filesystem driver names.
pub fn list_filesystem_types() -> Vec<&'static str> {
    let reg = FS_REGISTRY.lock();
    reg.keys().copied().collect()
}

/// Mounts a block device onto a VFS target directory using the specified filesystem driver.
pub fn mount_device(target_path: &str, dev_name: &str, fstype: &str) -> Result<PathNode> {
    let block_dev = get_block_device(dev_name).ok_or_else(|| {
        ostd::error!("VFS: Block device \"{}\" not found", dev_name);
        Errno::ENODEV
    })?;

    let fs_driver = get_filesystem_type(fstype).ok_or_else(|| {
        ostd::error!("VFS: Filesystem driver \"{}\" not registered", fstype);
        Errno::ENODEV
    })?;

    let fs_instance = fs_driver.mount(block_dev)?;
    let root_inode = fs_instance.root()?;

    let root_node = root()?;
    let target_node = lookup(root_node.clone(), root_node, target_path, LookupFlags::DIRECTORY)?;

    let mount_dentry = Arc::new(DEntry::new(
        String::from(target_path.trim_end_matches('/')),
        Some(root_inode),
        Some(Arc::downgrade(&target_node.dentry)),
    ));

    let new_mount = Arc::new(Mount::new(Arc::clone(&mount_dentry), fs_instance));
    target_node.mount(new_mount.clone())?;

    ostd::info!(
        "VFS: Successfully mounted {} ({}) at \"{}\"",
        dev_name,
        fstype,
        target_path
    );

    Ok(PathNode {
        mount: new_mount,
        dentry: mount_dentry,
    })
}

/// Automatically attempts to probe and mount any discovered block devices.
pub fn auto_probe_and_mount() {
    let devices = list_block_devices();
    if devices.is_empty() {
        return;
    }

    let drivers = {
        let reg = FS_REGISTRY.lock();
        reg.values().cloned().collect::<Vec<_>>()
    };

    for dev_name in devices {
        if let Some(dev) = get_block_device(&dev_name) {
            for driver in &drivers {
                match driver.mount(Arc::clone(&dev)) {
                    Ok(fs_instance) => {
                        let mount_point = format!("/mnt/{}", dev_name);
                        ostd::info!(
                            "VFS: Detected \"{}\" on {} ({} MB)",
                            driver.name(),
                            dev_name,
                            dev.capacity() / (1024 * 1024)
                        );

                        // Attempt to mount on /mnt if directory exists or at /data
                        if let Ok(root_node) = root() {
                            if let Ok(target_node) = lookup(
                                root_node.clone(),
                                root_node,
                                &mount_point,
                                LookupFlags::DIRECTORY,
                            ) {
                                if let Ok(root_inode) = fs_instance.root() {
                                    let mount_dentry = Arc::new(DEntry::new(
                                        mount_point.clone(),
                                        Some(root_inode),
                                        Some(Arc::downgrade(&target_node.dentry)),
                                    ));
                                    let new_mount =
                                        Arc::new(Mount::new(Arc::clone(&mount_dentry), fs_instance));
                                    let _ = target_node.mount(new_mount);
                                    ostd::info!("VFS: Auto-mounted {} on {}", dev_name, mount_point);
                                }
                            }
                        }
                        break;
                    }
                    Err(_) => {
                        // Driver could not mount this device, try next driver
                    }
                }
            }
        }
    }
}
