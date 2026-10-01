// SPDX-License-Identifier: GPL-2.0

//! Block Device Driver Abstraction.
//!
//! Provides the core [`BlockDevice`] trait required by filesystem implementations
//! such as `ext2`, `fat32`, or `exfat`, along with a global device registry.

pub mod nvme;

use alloc::{
    string::String,
    sync::Arc,
    vec::Vec,
};

pub use crate::drivers::BlockDevice;

/// Registers a block device into the global block layer.
pub fn register_block_device(name: &str, device: Arc<dyn BlockDevice>) {
    ostd::info!(
        "Block: Registered block device \"/dev/{}\" (size: {} MB, block_size: {} B)",
        name,
        device.capacity() / (1024 * 1024),
        device.block_size()
    );
    crate::drivers::register_blkdev(device);
}

/// Retrieves a block device by name.
pub fn get_block_device(name: &str) -> Option<Arc<dyn BlockDevice>> {
    crate::drivers::get_blkdevs().into_iter().find(|d| d.name() == name)
}

/// Lists all registered block device names.
pub fn list_block_devices() -> Vec<String> {
    crate::drivers::get_blkdevs().into_iter().map(|d| d.name()).collect()
}
