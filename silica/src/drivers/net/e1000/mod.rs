// SPDX-License-Identifier: GPL-2.0

//! Intel e1000 Gigabit Ethernet Network Driver.

pub mod device;
pub mod hw;
pub mod regs;

use alloc::format;
use ostd::io::IoMem;

use crate::{
    api::errno::{Errno, Result},
    drivers::{
        bus::pci::{self, PciDevice},
        net::register_network_device,
    },
};
pub use device::E1000Device;

/// Probes and initializes an Intel e1000 NIC discovered on the PCI bus.
pub fn probe_device(dev: &PciDevice, index: usize) -> Result<()> {
    ostd::info!("e1000: Initializing network controller at PCI {}", dev.address);

    // 1. Enable Bus Mastering (DMA) and Memory Space
    dev.enable_bus_mastering();
    dev.enable_memory_space();

    // 2. Locate BAR0 (MMIO region)
    let bar0 = dev.bars[0].ok_or_else(|| {
        ostd::error!("e1000: Controller has no valid BAR0");
        Errno::ENODEV
    })?;

    let mmio_range = (bar0.address as usize)..(bar0.address as usize + bar0.size as usize);
    let mmio = IoMem::acquire(mmio_range).map_err(|_| {
        ostd::error!("e1000: Failed to map BAR0 MMIO region");
        Errno::EIO
    })?;

    let name = format!("eth{}", index);
    let e1000 = E1000Device::new(name, mmio)?;

    // 3. Register device in drivers, devfs, and network stack
    register_network_device(e1000);

    Ok(())
}

fn init() -> Result<()> {
    let devices = pci::find_e1000_devices();
    if devices.is_empty() {
        ostd::info!("e1000: No Intel e1000 controller found on PCI bus");
        return Ok(());
    }

    for (i, dev) in devices.iter().enumerate() {
        if let Err(err) = probe_device(dev, i) {
            ostd::error!("e1000: Failed to initialize device {}: {:?}", dev.address, err);
        }
    }

    Ok(())
}

crate::module!(
    "Intel e1000 Gigabit Ethernet Driver",
    "SilicaOS Team",
    crate::modules::InitcallLevel::Device,
    init
);
