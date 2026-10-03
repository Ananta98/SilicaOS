// SPDX-License-Identifier: GPL-2.0

//! Intel e1000 Hardware Descriptors and Memory Structures.

pub const NUM_RX_DESCRIPTORS: usize = 64;
pub const NUM_TX_DESCRIPTORS: usize = 64;
pub const RX_BUFFER_SIZE: usize = 2048;
pub const TX_BUFFER_SIZE: usize = 2048;

/// 16-byte Receive Descriptor (Legacy format).
#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct RxDesc {
    pub addr: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

impl RxDesc {
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.addr.to_ne_bytes());
        b[8..10].copy_from_slice(&self.length.to_ne_bytes());
        b[10..12].copy_from_slice(&self.checksum.to_ne_bytes());
        b[12] = self.status;
        b[13] = self.errors;
        b[14..16].copy_from_slice(&self.special.to_ne_bytes());
        b
    }

    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self {
            addr: u64::from_ne_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            length: u16::from_ne_bytes([b[8], b[9]]),
            checksum: u16::from_ne_bytes([b[10], b[11]]),
            status: b[12],
            errors: b[13],
            special: u16::from_ne_bytes([b[14], b[15]]),
        }
    }
}

/// 16-byte Transmit Descriptor (Legacy format).
#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug)]
pub struct TxDesc {
    pub addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

impl TxDesc {
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.addr.to_ne_bytes());
        b[8..10].copy_from_slice(&self.length.to_ne_bytes());
        b[10] = self.cso;
        b[11] = self.cmd;
        b[12] = self.status;
        b[13] = self.css;
        b[14..16].copy_from_slice(&self.special.to_ne_bytes());
        b
    }

    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self {
            addr: u64::from_ne_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            length: u16::from_ne_bytes([b[8], b[9]]),
            cso: b[10],
            cmd: b[11],
            status: b[12],
            css: b[13],
            special: u16::from_ne_bytes([b[14], b[15]]),
        }
    }
}
