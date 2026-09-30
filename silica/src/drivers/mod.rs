// SPDX-License-Identifier: GPL-2.0

//! Unified Kernel Driver Model and Device Abstractions.
//!
//! Provides core [`Device`] and [`Driver`] traits, device registration,
//! bus discovery abstractions, and block device traits.

pub mod bus;
pub mod block;

use alloc::{
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::fmt;
use spin::Mutex;

use crate::{
    drivers::bus::pci::PciDevice,
    errno::Result,
};

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

/// Base trait implemented by all hardware and virtual devices in SilicaOS.
pub trait Device: Send + Sync {
    /// Identifier or device node name (e.g. "nvme0n1", "ttyS0").
    fn name(&self) -> String;

    /// Type category of this device.
    fn device_type(&self) -> DeviceType;
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

/// Global registry of active devices.
static DEVICE_REGISTRY: Mutex<Vec<Arc<dyn Device>>> = Mutex::new(Vec::new());

/// Register a device in the global device registry.
pub fn register_device(device: Arc<dyn Device>) {
    let mut reg = DEVICE_REGISTRY.lock();
    reg.push(device);
}

/// Returns a snapshot list of all registered devices.
pub fn list_devices() -> Vec<Arc<dyn Device>> {
    let reg = DEVICE_REGISTRY.lock();
    reg.clone()
}

/// Initializes the driver subsystem and probes system buses.
pub fn init() -> Result<()> {
    ostd::info!("Initializing driver subsystem...");
    bus::pci::init()?;
    Ok(())
}
