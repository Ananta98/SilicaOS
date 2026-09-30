// SPDX-License-Identifier: GPL-2.0

//! NVMe Command structures (SQE) and Completion Queue entries (CQE).

pub mod opcodes {
    // Admin Commands
    pub const DELETE_IO_SQ: u8 = 0x00;
    pub const CREATE_IO_SQ: u8 = 0x01;
    pub const DELETE_IO_CQ: u8 = 0x04;
    pub const CREATE_IO_CQ: u8 = 0x05;
    pub const IDENTIFY: u8 = 0x06;
    pub const SET_FEATURES: u8 = 0x09;

    // NVM (I/O) Commands
    pub const FLUSH: u8 = 0x00;
    pub const WRITE: u8 = 0x01;
    pub const READ: u8 = 0x02;
}

/// 64-byte NVMe Submission Queue Entry (SQE).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeSqe {
    pub cdw0: u32,       // Opcode [7:0], Fused [9:8], PSDT [15:14], CID [31:16]
    pub nsid: u32,       // Namespace Identifier
    pub reserved: u64,
    pub mptr: u64,       // Metadata Pointer
    pub dptr: [u64; 2],  // Data Pointer: PRP1 and PRP2
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

impl NvmeSqe {
    /// Builds an Identify command (CNS 0x01 = Controller, CNS 0x00 = Namespace).
    pub fn identify(nsid: u32, cns: u8, prp1: u64, cid: u16) -> Self {
        let mut sqe = Self::default();
        sqe.cdw0 = (opcodes::IDENTIFY as u32) | ((cid as u32) << 16);
        sqe.nsid = nsid;
        sqe.dptr[0] = prp1;
        sqe.cdw10 = cns as u32;
        sqe
    }

    /// Builds a Create I/O Completion Queue command.
    pub fn create_io_cq(cqid: u16, qsize: u16, prp1: u64, cid: u16) -> Self {
        let mut sqe = Self::default();
        sqe.cdw0 = (opcodes::CREATE_IO_CQ as u32) | ((cid as u32) << 16);
        sqe.dptr[0] = prp1;
        sqe.cdw10 = (cqid as u32) | (((qsize - 1) as u32) << 16);
        sqe.cdw11 = 0x0001; // Physically contiguous, interrupts disabled
        sqe
    }

    /// Builds a Create I/O Submission Queue command.
    pub fn create_io_sq(sqid: u16, cqid: u16, qsize: u16, prp1: u64, cid: u16) -> Self {
        let mut sqe = Self::default();
        sqe.cdw0 = (opcodes::CREATE_IO_SQ as u32) | ((cid as u32) << 16);
        sqe.dptr[0] = prp1;
        sqe.cdw10 = (sqid as u32) | (((qsize - 1) as u32) << 16);
        sqe.cdw11 = 0x0001 | ((cqid as u32) << 16); // Physically contiguous + CQ ID
        sqe
    }

    /// Builds an NVM Read command.
    pub fn read(nsid: u32, slba: u64, num_blocks: u16, prp1: u64, prp2: u64, cid: u16) -> Self {
        let mut sqe = Self::default();
        sqe.cdw0 = (opcodes::READ as u32) | ((cid as u32) << 16);
        sqe.nsid = nsid;
        sqe.dptr[0] = prp1;
        sqe.dptr[1] = prp2;
        sqe.cdw10 = (slba & 0xFFFF_FFFF) as u32;
        sqe.cdw11 = (slba >> 32) as u32;
        sqe.cdw12 = (num_blocks.saturating_sub(1)) as u32; // 0-based count
        sqe
    }

    /// Builds an NVM Write command.
    pub fn write(nsid: u32, slba: u64, num_blocks: u16, prp1: u64, prp2: u64, cid: u16) -> Self {
        let mut sqe = Self::default();
        sqe.cdw0 = (opcodes::WRITE as u32) | ((cid as u32) << 16);
        sqe.nsid = nsid;
        sqe.dptr[0] = prp1;
        sqe.dptr[1] = prp2;
        sqe.cdw10 = (slba & 0xFFFF_FFFF) as u32;
        sqe.cdw11 = (slba >> 32) as u32;
        sqe.cdw12 = (num_blocks.saturating_sub(1)) as u32; // 0-based count
        sqe
    }
}

/// 16-byte NVMe Completion Queue Entry (CQE).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmeCqe {
    pub cdw0: u32,       // Command-specific result
    pub reserved: u32,
    pub sq_head: u16,    // SQ Head Pointer
    pub sq_id: u16,      // SQ Identifier
    pub cid: u16,        // Command Identifier
    pub status: u16,     // Status field: Phase Tag is bit 0
}

impl NvmeCqe {
    /// Returns the Phase Tag bit (bit 0 of status).
    pub fn phase(&self) -> bool {
        (self.status & 1) != 0
    }

    /// Returns the Status Code (bits 14:1 of status).
    pub fn status_code(&self) -> u16 {
        (self.status >> 1) & 0x7FFF
    }

    /// Returns `true` if the command completed with no error.
    pub fn is_success(&self) -> bool {
        self.status_code() == 0
    }
}
