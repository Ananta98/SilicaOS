// SPDX-License-Identifier: GPL-2.0

//! Unified Kernel Driver Model and Device Abstractions.
//!
//! Provides core [`Device`] and [`Driver`] traits, device registration,
//! bus discovery abstractions, and block device traits.

pub mod block;
pub mod bus;
pub mod char;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt;
use spin::Mutex;

use crate::{drivers::bus::pci::PciDevice, errno::Result};

// ----------------------------------------------------------------------------
// Device Registries
// ----------------------------------------------------------------------------

pub static CHAR_DEVICES: Mutex<Vec<Arc<dyn CharacterDevice>>> = Mutex::new(Vec::new());
pub static BLK_DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

/// Base Device Trait
pub trait Device: Send + Sync {
    fn name(&self) -> String;
    fn device_type(&self) -> DeviceType;
}

/// A character device that provides unstructured read/write character streams.
pub trait CharacterDevice: Send + Sync {
    /// The unique name of the character device (e.g. "mouse", "kbd").
    fn name(&self) -> String;

    /// Read data from the device into the provided buffer.
    fn read(&self, buf: &mut [u8]) -> Result<usize>;

    /// Write data to the device from the provided buffer.
    fn write(&self, buf: &[u8]) -> Result<usize>;

    /// Optional: Performs an I/O control operation.
    fn ioctl(&self, _cmd: u32, _arg: usize) -> Result<usize> {
        crate::return_errno!(EINVAL, "ioctl not supported")
    }
}

/// A block device that provides structured read/write operations for blocks.
pub trait BlockDevice: Send + Sync {
    /// The unique name of the block device (e.g. "nvme0n1").
    fn name(&self) -> String;

    /// Return the logical block size of the device in bytes (e.g. 512 or 4096).
    fn block_size(&self) -> usize;

    /// Return the total number of logical blocks on the device.
    fn block_count(&self) -> u64;

    /// Total capacity of the device in bytes.
    fn capacity(&self) -> u64 {
        self.block_count() * (self.block_size() as u64)
    }

    /// Read blocks from the device.
    fn read_blocks(&self, block_id: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write blocks to the device.
    fn write_blocks(&self, block_id: u64, buf: &[u8]) -> Result<usize>;

    /// Optional: Flush written blocks.
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Category / type of the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    /// Block storage device (NVMe, SATA, RAM disk)
    Block,
    /// Character device (UART, TTY, random)
    Char,
    /// Network interface (Ethernet, Wi-Fi)
    Network,
    /// System bus or bridge (PCI, USB, I2C)
    Bus,
    /// Display or graphics controller
    Display,
}

impl fmt::Display for DeviceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block => write!(f, "block"),
            Self::Char => write!(f, "char"),
            Self::Network => write!(f, "network"),
            Self::Bus => write!(f, "bus"),
            Self::Display => write!(f, "display"),
        }
    }
}

/// Base trait for hardware device drivers capable of binding to devices.
pub trait Driver: Send + Sync {
    /// Human-readable name of the driver (e.g. "nvme", "e1000").
    fn name(&self) -> &'static str;

    /// Probe a discovered PCI device to determine if this driver can manage it.
    fn probe_pci(&self, dev: &PciDevice) -> Result<bool> {
        let _ = dev;
        Ok(false)
    }
}

/// Register a Character Device in the global registry.
pub fn register_chrdev(device: Arc<dyn CharacterDevice>) {
    let mut reg = CHAR_DEVICES.lock();
    reg.push(device);
}

/// Register a Block Device in the global registry.
pub fn register_blkdev(device: Arc<dyn BlockDevice>) {
    let mut reg = BLK_DEVICES.lock();
    reg.push(device);
}

/// Returns a snapshot list of all registered Character Devices.
pub fn get_chrdevs() -> Vec<Arc<dyn CharacterDevice>> {
    let reg = CHAR_DEVICES.lock();
    reg.clone()
}

/// Returns a snapshot list of all registered Block Devices.
pub fn get_blkdevs() -> Vec<Arc<dyn BlockDevice>> {
    let reg = BLK_DEVICES.lock();
    reg.clone()
}

/// Initializes the driver subsystem and probes system buses.
pub fn init() -> Result<()> {
    ostd::info!("Initializing driver subsystem...");
    bus::pci::init()?;
    Ok(())
}
