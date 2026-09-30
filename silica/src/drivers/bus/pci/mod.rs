// SPDX-License-Identifier: GPL-2.0

//! PCI / PCIe Bus Subsystem.
//!
//! Organizes PCI functionalities into focused submodules:
//! - [`address`]: PCI device addressing (`Bus:Device.Function`).
//! - [`config`]: Low-level PCI configuration space port I/O and register definitions.
//! - [`bar`]: Base Address Register (BAR) decoding and probing.
//! - [`device`]: Discovered PCI device model and control helpers.
//! - [`scan`]: PCI bus enumeration.
//! - [`ids`]: Standard PCI class and subclass IDs.

pub mod address;
pub mod bar;
pub mod config;
pub mod device;
pub mod ids;
pub mod scan;

use alloc::vec::Vec;
use spin::Mutex;

pub use address::PciAddress;
pub use bar::{BarType, PciBar, probe_bar};
pub use config::{
    PCI_BAR0, PCI_CLASS_CODE, PCI_COMMAND, PCI_COMMAND_BUS_MASTER,
    PCI_COMMAND_INTX_DISABLE, PCI_COMMAND_IO_SPACE, PCI_COMMAND_MEMORY_SPACE,
    PCI_CONFIG_ADDRESS_PORT, PCI_CONFIG_DATA_PORT, PCI_DEVICE_ID, PCI_HEADER_TYPE,
    PCI_INTERRUPT_LINE, PCI_INTERRUPT_PIN, PCI_PROG_IF, PCI_REVISION_ID, PCI_STATUS,
    PCI_SUBCLASS, PCI_VENDOR_ID, pci_read_config_u16, pci_read_config_u32,
    pci_read_config_u8, pci_write_config_u16, pci_write_config_u32, pci_write_config_u8,
};
pub use device::PciDevice;
pub use scan::scan_bus;

use crate::errno::Result;

/// Global registry of discovered PCI devices.
pub static PCI_DEVICES: Mutex<Vec<PciDevice>> = Mutex::new(Vec::new());

/// Initializes the PCI subsystem, enumerating all connected devices.
pub fn init() -> Result<()> {
    ostd::info!("Probing PCI bus...");
    let devices = scan_bus();

    ostd::info!("PCI: Discovered {} device(s)", devices.len());
    for dev in &devices {
        ostd::info!(
            "PCI: [{}] {:04x}:{:04x} class {:02x}:{:02x} (prog_if {:02x}) - {}",
            dev.address,
            dev.vendor_id,
            dev.device_id,
            dev.class_code,
            dev.subclass,
            dev.prog_if,
            if dev.is_nvme() { "NVMe Controller" } else { "Generic Device" }
        );

        for bar in dev.bars.iter().flatten() {
            ostd::info!(
                "  BAR{}: {:?} base {:#x} (size: {} KB)",
                bar.index,
                bar.bar_type,
                bar.address,
                bar.size / 1024
            );
        }
    }

    *PCI_DEVICES.lock() = devices;
    Ok(())
}

/// Query all discovered PCI devices.
pub fn get_devices() -> Vec<PciDevice> {
    PCI_DEVICES.lock().clone()
}

/// Find the first NVMe storage controller on the PCI bus, if present.
pub fn find_nvme_device() -> Option<PciDevice> {
    PCI_DEVICES.lock().iter().find(|d| d.is_nvme()).cloned()
}
