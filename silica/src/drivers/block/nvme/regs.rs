// SPDX-License-Identifier: GPL-2.0

//! NVMe Controller Memory Mapped I/O Registers and Bitfields.

// Standard NVMe Controller Register Offsets
pub const NVME_REG_CAP: usize = 0x0000;   // Controller Capabilities (64-bit)
pub const NVME_REG_VS: usize = 0x0008;    // Version (32-bit)
pub const NVME_REG_INTMS: usize = 0x000C; // Interrupt Mask Set (32-bit)
pub const NVME_REG_INTMC: usize = 0x0010; // Interrupt Mask Clear (32-bit)
pub const NVME_REG_CC: usize = 0x0014;    // Controller Configuration (32-bit)
pub const NVME_REG_CSTS: usize = 0x001C;  // Controller Status (32-bit)
pub const NVME_REG_AQA: usize = 0x0024;   // Admin Queue Attributes (32-bit)
pub const NVME_REG_ASQ: usize = 0x0028;   // Admin Submission Queue Base (64-bit)
pub const NVME_REG_ACQ: usize = 0x0030;   // Admin Completion Queue Base (64-bit)

// CC (Controller Configuration) Bitfields
pub const NVME_CC_EN: u32 = 1 << 0;       // Enable Controller
pub const NVME_CC_CSS_NVM: u32 = 0 << 4;  // NVM Command Set
pub const NVME_CC_MPS_4K: u32 = 0 << 7;   // 4KB Memory Page Size (2^(12 + 0))
pub const NVME_CC_IOSQES_64: u32 = 6 << 16; // 64-byte I/O SQ Entry Size (2^6)
pub const NVME_CC_IOCQES_16: u32 = 4 << 20; // 16-byte I/O CQ Entry Size (2^4)

// CSTS (Controller Status) Bitfields
pub const NVME_CSTS_RDY: u32 = 1 << 0;    // Controller Ready
pub const NVME_CSTS_CFS: u32 = 1 << 1;    // Controller Fatal Status

// Doorbell Base Offset and Stride Calculation
pub const NVME_DOORBELL_BASE: usize = 0x1000;

/// Calculates the Submission Queue Tail Doorbell register offset.
pub fn sq_doorbell_offset(qid: u16, stride_bytes: usize) -> usize {
    NVME_DOORBELL_BASE + (2 * qid as usize * stride_bytes)
}

/// Calculates the Completion Queue Head Doorbell register offset.
pub fn cq_doorbell_offset(qid: u16, stride_bytes: usize) -> usize {
    NVME_DOORBELL_BASE + ((2 * qid as usize + 1) * stride_bytes)
}
