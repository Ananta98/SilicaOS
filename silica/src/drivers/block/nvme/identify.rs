// SPDX-License-Identifier: GPL-2.0

//! NVMe Identify Controller and Namespace data structures.

use alloc::string::String;
use core::str;

/// LBA Format structure (part of Identify Namespace).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct LbaFormat {
    pub ms: u16,  // Metadata Size
    pub ds: u8,   // LBA Data Size (2^ds bytes, e.g. 9 = 512 bytes, 12 = 4096 bytes)
    pub rp: u8,   // Relative Performance
}

/// 4096-byte Identify Controller data structure.
#[repr(C)]
pub struct IdentifyController {
    pub vid: u16,
    pub ssvid: u16,
    pub sn: [u8; 20],        // Serial Number
    pub mn: [u8; 40],        // Model Number
    pub fr: [u8; 8],         // Firmware Revision
    pub rab: u8,
    pub ieee: [u8; 3],
    pub cmic: u8,
    pub mdts: u8,
    pub cntlid: u16,
    pub ver: u32,
    pub rtd3r: u32,
    pub rtd3e: u32,
    pub oaes: u32,
    pub ctratt: u32,
    pub rsvd64: [u8; 192],
    pub nn: u32,             // Number of Namespaces
    pub rsvd260: [u8; 3836],
}

impl IdentifyController {
    /// Return the model number as a sanitized string.
    pub fn model_number(&self) -> String {
        str::from_utf8(&self.mn)
            .unwrap_or("Unknown NVMe")
            .trim()
            .into()
    }

    /// Return the serial number as a sanitized string.
    pub fn serial_number(&self) -> String {
        str::from_utf8(&self.sn)
            .unwrap_or("")
            .trim()
            .into()
    }
}

/// 4096-byte Identify Namespace data structure.
#[repr(C)]
pub struct IdentifyNamespace {
    pub nsze: u64,           // Namespace Size (total blocks)
    pub ncap: u64,           // Namespace Capacity (allocatable blocks)
    pub nuse: u64,           // Namespace Utilization
    pub nsfeat: u8,
    pub nlbaf: u8,           // Number of LBA Formats
    pub flbas: u8,           // Formatted LBA Size (bits 3:0 index into lbaf)
    pub mc: u8,
    pub dpc: u8,
    pub dps: u8,
    pub nmic: u8,
    pub rescap: u8,
    pub fpi: u8,
    pub dlfeat: u8,
    pub nawun: u16,
    pub nawupf: u16,
    pub nacwu: u16,
    pub nabsn: u16,
    pub nabofr: u16,
    pub nabsu: u16,
    pub naxds: u16,
    pub rsvd40: [u8; 88],
    pub lbaf: [LbaFormat; 16],
    pub rsvd192: [u8; 3904],
}

impl IdentifyNamespace {
    /// Returns the logical block size in bytes (e.g. 512 or 4096).
    pub fn block_size(&self) -> usize {
        let format_idx = (self.flbas & 0x0F) as usize;
        let ds = self.lbaf[format_idx].ds;
        if ds >= 9 && ds <= 16 {
            1 << ds
        } else {
            512 // Default fallback
        }
    }

    /// Returns the total logical block count.
    pub fn block_count(&self) -> u64 {
        self.nsze
    }
}
