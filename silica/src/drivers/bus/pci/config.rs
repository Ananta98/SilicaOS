// SPDX-License-Identifier: GPL-2.0

//! PCI Configuration Space access and standard register definitions.

use spin::Mutex;

#[cfg(target_arch = "x86_64")]
use ostd::arch::device::io_port::ReadWriteAccess;
#[cfg(target_arch = "x86_64")]
use ostd::io::IoPort;

use super::address::PciAddress;

pub const PCI_CONFIG_ADDRESS_PORT: u16 = 0xCF8;
pub const PCI_CONFIG_DATA_PORT: u16 = 0xCFC;

// Standard PCI Configuration Header 0x00 Register Offsets
pub const PCI_VENDOR_ID: u8 = 0x00;
pub const PCI_DEVICE_ID: u8 = 0x02;
pub const PCI_COMMAND: u8 = 0x04;
pub const PCI_STATUS: u8 = 0x06;
pub const PCI_REVISION_ID: u8 = 0x08;
pub const PCI_PROG_IF: u8 = 0x09;
pub const PCI_SUBCLASS: u8 = 0x0A;
pub const PCI_CLASS_CODE: u8 = 0x0B;
pub const PCI_HEADER_TYPE: u8 = 0x0E;
pub const PCI_BAR0: u8 = 0x10;
pub const PCI_INTERRUPT_LINE: u8 = 0x3C;
pub const PCI_INTERRUPT_PIN: u8 = 0x3D;

// PCI Command Register Bit Flags
pub const PCI_COMMAND_IO_SPACE: u16 = 1 << 0;
pub const PCI_COMMAND_MEMORY_SPACE: u16 = 1 << 1;
pub const PCI_COMMAND_BUS_MASTER: u16 = 1 << 2;
pub const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

#[cfg(target_arch = "x86_64")]
struct PciPorts {
    addr_port: IoPort<u32, ReadWriteAccess>,
    data_port: IoPort<u32, ReadWriteAccess>,
}

#[cfg(target_arch = "x86_64")]
static PCI_PORTS: Mutex<Option<PciPorts>> = Mutex::new(None);

fn ensure_pci_ports() {
    #[cfg(target_arch = "x86_64")]
    {
        let mut guard = PCI_PORTS.lock();
        if guard.is_none() {
            let addr = IoPort::acquire_overlapping(PCI_CONFIG_ADDRESS_PORT)
                .expect("failed to acquire PCI config address port 0xCF8");
            let data = IoPort::acquire_overlapping(PCI_CONFIG_DATA_PORT)
                .expect("failed to acquire PCI config data port 0xCFC");
            *guard = Some(PciPorts { addr_port: addr, data_port: data });
        }
    }
}

fn config_address(addr: PciAddress, offset: u8) -> u32 {
    0x8000_0000
        | ((addr.bus as u32) << 16)
        | ((addr.device as u32) << 11)
        | ((addr.function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

/// Read a 32-bit dword from PCI configuration space.
pub fn pci_read_config_u32(addr: PciAddress, offset: u8) -> u32 {
    ensure_pci_ports();
    #[cfg(target_arch = "x86_64")]
    {
        let guard = PCI_PORTS.lock();
        if let Some(ref ports) = *guard {
            ports.addr_port.write(config_address(addr, offset));
            return ports.data_port.read();
        }
    }
    0xFFFF_FFFF
}

/// Read a 16-bit word from PCI configuration space.
pub fn pci_read_config_u16(addr: PciAddress, offset: u8) -> u16 {
    let dword = pci_read_config_u32(addr, offset);
    let shift = ((offset & 2) * 8) as u32;
    ((dword >> shift) & 0xFFFF) as u16
}

/// Read an 8-bit byte from PCI configuration space.
pub fn pci_read_config_u8(addr: PciAddress, offset: u8) -> u8 {
    let dword = pci_read_config_u32(addr, offset);
    let shift = ((offset & 3) * 8) as u32;
    ((dword >> shift) & 0xFF) as u8
}

/// Write a 32-bit dword to PCI configuration space.
pub fn pci_write_config_u32(addr: PciAddress, offset: u8, val: u32) {
    ensure_pci_ports();
    #[cfg(target_arch = "x86_64")]
    {
        let guard = PCI_PORTS.lock();
        if let Some(ref ports) = *guard {
            ports.addr_port.write(config_address(addr, offset));
            ports.data_port.write(val);
        }
    }
}

/// Write a 16-bit word to PCI configuration space.
pub fn pci_write_config_u16(addr: PciAddress, offset: u8, val: u16) {
    let aligned_offset = offset & 0xFC;
    let current = pci_read_config_u32(addr, aligned_offset);
    let shift = ((offset & 2) * 8) as u32;
    let mask = !(0xFFFF << shift);
    let new_val = (current & mask) | (((val as u32) & 0xFFFF) << shift);
    pci_write_config_u32(addr, aligned_offset, new_val);
}

/// Write an 8-bit byte to PCI configuration space.
pub fn pci_write_config_u8(addr: PciAddress, offset: u8, val: u8) {
    let aligned_offset = offset & 0xFC;
    let current = pci_read_config_u32(addr, aligned_offset);
    let shift = ((offset & 3) * 8) as u32;
    let mask = !(0xFF << shift);
    let new_val = (current & mask) | (((val as u32) & 0xFF) << shift);
    pci_write_config_u32(addr, aligned_offset, new_val);
}
