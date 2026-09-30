// SPDX-License-Identifier: GPL-2.0

//! PCI Address representation.

use core::fmt;

/// Represents a physical PCI Device Address (Bus:Device.Function).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PciAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciAddress {
    /// Create a new PCI address.
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self { bus, device, function }
    }
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{:x}", self.bus, self.device, self.function)
    }
}
