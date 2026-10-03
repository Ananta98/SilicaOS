// SPDX-License-Identifier: GPL-2.0

//! PCI Device representation and helper control methods.

use super::{
    address::PciAddress,
    bar::PciBar,
    config::{
        PCI_COMMAND, PCI_COMMAND_BUS_MASTER, PCI_COMMAND_MEMORY_SPACE, pci_read_config_u8,
        pci_read_config_u16, pci_read_config_u32, pci_write_config_u8, pci_write_config_u16,
        pci_write_config_u32,
    },
    ids,
};

/// Represents a discovered PCI device.
#[derive(Debug, Clone)]
pub struct PciDevice {
    pub address: PciAddress,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub bars: [Option<PciBar>; 6],
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
}

impl PciDevice {
    /// Read an 8-bit byte from this device's configuration space.
    pub fn read_config_u8(&self, offset: u8) -> u8 {
        pci_read_config_u8(self.address, offset)
    }

    /// Read a 16-bit word from this device's configuration space.
    pub fn read_config_u16(&self, offset: u8) -> u16 {
        pci_read_config_u16(self.address, offset)
    }

    /// Read a 32-bit dword from this device's configuration space.
    pub fn read_config_u32(&self, offset: u8) -> u32 {
        pci_read_config_u32(self.address, offset)
    }

    /// Write an 8-bit byte to this device's configuration space.
    pub fn write_config_u8(&self, offset: u8, val: u8) {
        pci_write_config_u8(self.address, offset, val);
    }

    /// Write a 16-bit word to this device's configuration space.
    pub fn write_config_u16(&self, offset: u8, val: u16) {
        pci_write_config_u16(self.address, offset, val);
    }

    /// Write a 32-bit dword to this device's configuration space.
    pub fn write_config_u32(&self, offset: u8, val: u32) {
        pci_write_config_u32(self.address, offset, val);
    }

    /// Enables Bus Mastering (required for DMA engines such as NVMe).
    pub fn enable_bus_mastering(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND);
        self.write_config_u16(PCI_COMMAND, cmd | PCI_COMMAND_BUS_MASTER);
    }

    /// Enables Memory Mapped I/O space access.
    pub fn enable_memory_space(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND);
        self.write_config_u16(PCI_COMMAND, cmd | PCI_COMMAND_MEMORY_SPACE);
    }

    /// Returns `true` if this device is an NVM Express (NVMe) storage controller.
    pub fn is_nvme(&self) -> bool {
        self.class_code == ids::class::MASS_STORAGE
            && self.subclass == ids::mass_storage::NON_VOLATILE_MEMORY
            && self.prog_if == ids::mass_storage::nvm::NVME
    }

    /// Returns `true` if this device is an Intel e1000/e1000e network controller.
    pub fn is_e1000(&self) -> bool {
        self.vendor_id == 0x8086
            && matches!(
                self.device_id,
                0x100e | 0x1004 | 0x100f | 0x1010 | 0x1019 | 0x101a | 0x107c | 0x10d3 | 0x153a | 0x1502
            )
    }
}

