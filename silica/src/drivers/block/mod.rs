// SPDX-License-Identifier: GPL-2.0

//! Block Device Driver Abstraction.
//!
//! Provides the core [`BlockDevice`] trait required by filesystem implementations
//! such as `ext2`, `fat32`, or `exfat`, along with a global device registry.

pub mod nvme;

use alloc::{
    borrow::ToOwned,
    collections::btree_map::BTreeMap,
    string::String,
    sync::Arc,
    vec::Vec,
};
use spin::Mutex;

use crate::{
    drivers::Device,
    errno::Result,
};

/// Trait implemented by storage devices capable of block-level I/O.
pub trait BlockDevice: Device + Send + Sync {
    /// Return the logical block size of the device in bytes (e.g. 512 or 4096).
    fn block_size(&self) -> usize;

    /// Return the total number of logical blocks on the device.
    fn block_count(&self) -> u64;

    /// Total capacity of the device in bytes.
    fn capacity(&self) -> u64 {
        self.block_count() * (self.block_size() as u64)
    }

    /// Read blocks starting at `start_block` into `buf`.
    ///
    /// The buffer length must be a multiple of [`Self::block_size`].
    fn read_blocks(&self, start_block: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write blocks starting at `start_block` from `buf`.
    ///
    /// The buffer length must be a multiple of [`Self::block_size`].
    fn write_blocks(&self, start_block: u64, buf: &[u8]) -> Result<usize>;

    /// Flushes any write cache present in the device hardware or driver.
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Global registry of registered block devices (e.g. "nvme0n1", "sda").
static BLOCK_DEVICES: Mutex<BTreeMap<String, Arc<dyn BlockDevice>>> = Mutex::new(BTreeMap::new());

/// Registers a block device into the global block layer.
pub fn register_block_device(name: &str, device: Arc<dyn BlockDevice>) {
    ostd::info!(
        "Block: Registered block device \"/dev/{}\" (size: {} MB, block_size: {} B)",
        name,
        device.capacity() / (1024 * 1024),
        device.block_size()
    );
    let mut table = BLOCK_DEVICES.lock();
    table.insert(name.to_owned(), device);
}

/// Retrieves a block device by name.
pub fn get_block_device(name: &str) -> Option<Arc<dyn BlockDevice>> {
    let table = BLOCK_DEVICES.lock();
    table.get(name).cloned()
}

/// Lists all registered block device names.
pub fn list_block_devices() -> Vec<String> {
    let table = BLOCK_DEVICES.lock();
    table.keys().cloned().collect()
}
