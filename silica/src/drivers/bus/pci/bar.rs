// SPDX-License-Identifier: GPL-2.0

//! PCI Base Address Register (BAR) structures and probing logic.

use super::{
    address::PciAddress,
    config::{PCI_BAR0, pci_read_config_u32, pci_write_config_u32},
};

/// Type of Base Address Register (BAR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarType {
    /// 32-bit Memory mapped I/O region
    Memory32,
    /// 64-bit Memory mapped I/O region
    Memory64,
    /// Legacy Port I/O region
    Io,
}

/// Represents a decoded PCI Base Address Register (BAR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciBar {
    pub index: u8,
    pub bar_type: BarType,
    pub address: u64,
    pub size: u64,
    pub prefetchable: bool,
}

/// Probes a BAR register at `bar_idx` (0..6) for a given device.
///
/// Returns `(Option<PciBar>, step)` where `step` indicates how many BAR registers
/// were consumed (1 for 32-bit or I/O BARs, 2 for 64-bit Memory BARs).
pub fn probe_bar(addr: PciAddress, bar_idx: u8) -> (Option<PciBar>, u8) {
    let bar_offset = PCI_BAR0 + (bar_idx * 4);
    let raw_val = pci_read_config_u32(addr, bar_offset);

    if raw_val == 0 || raw_val == 0xFFFF_FFFF {
        return (None, 1);
    }

    let is_io = (raw_val & 1) != 0;

    if is_io {
        // I/O Space BAR
        pci_write_config_u32(addr, bar_offset, 0xFFFF_FFFF);
        let size_mask = pci_read_config_u32(addr, bar_offset);
        pci_write_config_u32(addr, bar_offset, raw_val);

        let size = (!(size_mask & !0x3)).wrapping_add(1) as u64;
        let base = (raw_val & !0x3) as u64;

        (
            Some(PciBar {
                index: bar_idx,
                bar_type: BarType::Io,
                address: base,
                size,
                prefetchable: false,
            }),
            1,
        )
    } else {
        // Memory Space BAR
        let mem_type = (raw_val >> 1) & 0x3;
        let prefetchable = (raw_val & (1 << 3)) != 0;

        if mem_type == 2 {
            // 64-bit Memory Space BAR (occupies two consecutive 32-bit registers)
            let raw_high = pci_read_config_u32(addr, bar_offset + 4);
            let raw_64 = ((raw_high as u64) << 32) | ((raw_val & !0xF) as u64);

            pci_write_config_u32(addr, bar_offset, 0xFFFF_FFFF);
            pci_write_config_u32(addr, bar_offset + 4, 0xFFFF_FFFF);
            let size_low = pci_read_config_u32(addr, bar_offset) & !0xF;
            let size_high = pci_read_config_u32(addr, bar_offset + 4);
            pci_write_config_u32(addr, bar_offset, raw_val);
            pci_write_config_u32(addr, bar_offset + 4, raw_high);

            let size_mask_64 = ((size_high as u64) << 32) | (size_low as u64);
            let size = (!size_mask_64).wrapping_add(1);

            (
                Some(PciBar {
                    index: bar_idx,
                    bar_type: BarType::Memory64,
                    address: raw_64,
                    size,
                    prefetchable,
                }),
                2,
            )
        } else {
            // 32-bit Memory Space BAR
            pci_write_config_u32(addr, bar_offset, 0xFFFF_FFFF);
            let size_mask = pci_read_config_u32(addr, bar_offset) & !0xF;
            pci_write_config_u32(addr, bar_offset, raw_val);

            let size = (!size_mask).wrapping_add(1) as u64;
            let base = (raw_val & !0xF) as u64;

            (
                Some(PciBar {
                    index: bar_idx,
                    bar_type: BarType::Memory32,
                    address: base,
                    size,
                    prefetchable,
                }),
                1,
            )
        }
    }
}
