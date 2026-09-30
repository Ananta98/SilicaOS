// SPDX-License-Identifier: GPL-2.0

//! PCI Bus enumeration and device scanning.

use alloc::vec::Vec;

use super::{
    address::PciAddress,
    bar::probe_bar,
    config::{
        PCI_CLASS_CODE, PCI_DEVICE_ID, PCI_HEADER_TYPE, PCI_INTERRUPT_LINE,
        PCI_INTERRUPT_PIN, PCI_PROG_IF, PCI_REVISION_ID, PCI_SUBCLASS, PCI_VENDOR_ID,
        pci_read_config_u16, pci_read_config_u8,
    },
    device::PciDevice,
};

/// Scans all PCI buses and returns all discovered, active devices.
pub fn scan_bus() -> Vec<PciDevice> {
    let mut devices = Vec::new();

    // Standard scan: Buses 0..=32, Devices 0..32, Functions 0..8
    for bus in 0..=32 {
        for device in 0..32 {
            let addr_f0 = PciAddress::new(bus, device, 0);
            let vendor_id = pci_read_config_u16(addr_f0, PCI_VENDOR_ID);

            // 0xFFFF indicates device not present, 0x0000 indicates invalid/unmapped
            if vendor_id == 0xFFFF || vendor_id == 0x0000 {
                continue;
            }

            let header_type = pci_read_config_u8(addr_f0, PCI_HEADER_TYPE);
            let is_multi_function = (header_type & 0x80) != 0;
            let max_functions = if is_multi_function { 8 } else { 1 };

            for function in 0..max_functions {
                let addr = PciAddress::new(bus, device, function);
                let ven_id = pci_read_config_u16(addr, PCI_VENDOR_ID);

                if ven_id == 0xFFFF || ven_id == 0x0000 {
                    continue;
                }

                let dev_id = pci_read_config_u16(addr, PCI_DEVICE_ID);
                let revision = pci_read_config_u8(addr, PCI_REVISION_ID);
                let prog_if = pci_read_config_u8(addr, PCI_PROG_IF);
                let subclass = pci_read_config_u8(addr, PCI_SUBCLASS);
                let class_code = pci_read_config_u8(addr, PCI_CLASS_CODE);
                let hdr_type = pci_read_config_u8(addr, PCI_HEADER_TYPE) & 0x7F;

                // Probe BARs only for Standard Header (Type 0x00)
                let mut bars = [None; 6];
                if hdr_type == 0x00 {
                    let mut b = 0;
                    while b < 6 {
                        let (bar, step) = probe_bar(addr, b);
                        if let Some(bar_info) = bar {
                            bars[b as usize] = Some(bar_info);
                        }
                        b += step;
                    }
                }

                let int_line = pci_read_config_u8(addr, PCI_INTERRUPT_LINE);
                let int_pin = pci_read_config_u8(addr, PCI_INTERRUPT_PIN);

                devices.push(PciDevice {
                    address: addr,
                    vendor_id: ven_id,
                    device_id: dev_id,
                    class_code,
                    subclass,
                    prog_if,
                    revision,
                    header_type: hdr_type,
                    bars,
                    interrupt_line: int_line,
                    interrupt_pin: int_pin,
                });
            }
        }
    }

    devices
}
