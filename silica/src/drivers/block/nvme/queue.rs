// SPDX-License-Identifier: GPL-2.0

//! NVMe Submission and Completion Queue pair management and DMA execution.

use core::mem::size_of;

use ostd::{
    io::IoMem,
    mm::{
        HasPaddr, VmIoOnce,
        dma::DmaCoherent,
        io::util::HasVmReaderWriter,
    },
};

use super::{
    commands::{NvmeCqe, NvmeSqe},
    regs::{cq_doorbell_offset, sq_doorbell_offset},
};
use crate::errno::{Errno, Result};

pub const DEFAULT_QUEUE_SIZE: u16 = 64;

/// Manages a paired NVMe Submission Queue and Completion Queue.
pub struct NvmeQueuePair {
    pub qid: u16,
    pub qsize: u16,
    sq_dma: DmaCoherent,
    cq_dma: DmaCoherent,
    pub sq_paddr: u64,
    pub cq_paddr: u64,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: bool,
    sq_doorbell: usize,
    cq_doorbell: usize,
    cid_counter: u16,
}

impl NvmeQueuePair {
    /// Creates and initializes a new DMA-backed queue pair.
    pub fn new(qid: u16, qsize: u16, stride_bytes: usize) -> Result<Self> {
        let sq_bytes = (qsize as usize) * size_of::<NvmeSqe>();
        let cq_bytes = (qsize as usize) * size_of::<NvmeCqe>();

        let sq_frames = (sq_bytes + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let cq_frames = (cq_bytes + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;

        let sq_dma = DmaCoherent::alloc(sq_frames, true).map_err(|_| Errno::ENOMEM)?;
        let cq_dma = DmaCoherent::alloc(cq_frames, true).map_err(|_| Errno::ENOMEM)?;

        let sq_paddr = sq_dma.paddr() as u64;
        let cq_paddr = cq_dma.paddr() as u64;

        let sq_doorbell = sq_doorbell_offset(qid, stride_bytes);
        let cq_doorbell = cq_doorbell_offset(qid, stride_bytes);

        Ok(Self {
            qid,
            qsize,
            sq_dma,
            cq_dma,
            sq_paddr,
            cq_paddr,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: true, // Initial phase expected from hardware is 1
            sq_doorbell,
            cq_doorbell,
            cid_counter: 0,
        })
    }

    /// Allocates a unique command identifier for submission.
    pub fn next_cid(&mut self) -> u16 {
        let cid = self.cid_counter;
        self.cid_counter = self.cid_counter.wrapping_add(1);
        cid
    }

    /// Writes an SQE entry into the DMA buffer safely without unsafe code.
    fn write_sqe(&self, slot: usize, sqe: &NvmeSqe) -> Result<()> {
        let base = slot * size_of::<NvmeSqe>();
        let mut writer = self.sq_dma.writer();
        writer.skip(base);
        writer.write_val(&sqe.cdw0).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.nsid).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.reserved).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.mptr).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.dptr[0]).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.dptr[1]).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.cdw10).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.cdw11).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.cdw12).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.cdw13).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.cdw14).map_err(|_| Errno::EIO)?;
        writer.write_val(&sqe.cdw15).map_err(|_| Errno::EIO)?;
        Ok(())
    }

    /// Reads a CQE entry from the DMA buffer safely without unsafe code.
    fn read_cqe(&self, slot: usize) -> Result<NvmeCqe> {
        let base = slot * size_of::<NvmeCqe>();
        let mut reader = self.cq_dma.reader();
        reader.skip(base);
        let cdw0: u32 = reader.read_val().map_err(|_| Errno::EIO)?;
        let reserved: u32 = reader.read_val().map_err(|_| Errno::EIO)?;
        let sq_head: u16 = reader.read_val().map_err(|_| Errno::EIO)?;
        let sq_id: u16 = reader.read_val().map_err(|_| Errno::EIO)?;
        let cid: u16 = reader.read_val().map_err(|_| Errno::EIO)?;
        let status: u16 = reader.read_val().map_err(|_| Errno::EIO)?;
        Ok(NvmeCqe {
            cdw0,
            reserved,
            sq_head,
            sq_id,
            cid,
            status,
        })
    }

    /// Submits an SQE, rings the submission doorbell, and polls for completion.
    pub fn submit_and_wait(&mut self, mut sqe: NvmeSqe, mmio: &IoMem) -> Result<NvmeCqe> {
        let cid = self.next_cid();
        sqe.cdw0 = (sqe.cdw0 & 0x0000_FFFF) | ((cid as u32) << 16);

        // 1. Write SQE into DMA buffer
        self.write_sqe(self.sq_tail as usize, &sqe)?;

        // 2. Advance SQ tail
        self.sq_tail = (self.sq_tail + 1) % self.qsize;

        // 3. Ring SQ Doorbell (notify hardware)
        let sq_tail_val = self.sq_tail as u32;
        mmio.write_once(self.sq_doorbell, &sq_tail_val).map_err(|_| Errno::EIO)?;

        // 4. Poll CQE at cq_head for completion
        let mut timeout_spins = 2_000_000usize;

        loop {
            let cqe = self.read_cqe(self.cq_head as usize)?;

            // Check if hardware inverted phase bit to indicate completion
            if cqe.phase() == self.cq_phase {
                // 5. Advance CQ Head
                self.cq_head = (self.cq_head + 1) % self.qsize;
                if self.cq_head == 0 {
                    self.cq_phase = !self.cq_phase;
                }

                // 6. Ring CQ Head Doorbell
                let cq_head_val = self.cq_head as u32;
                mmio.write_once(self.cq_doorbell, &cq_head_val).map_err(|_| Errno::EIO)?;

                // 7. Verify Status
                if !cqe.is_success() {
                    ostd::error!(
                        "NVMe command failed: cid={}, status_code={:#x}",
                        cqe.cid,
                        cqe.status_code()
                    );
                    crate::return_errno!(EIO, "nvme command completion error");
                }

                return Ok(cqe);
            }

            core::hint::spin_loop();
            timeout_spins = timeout_spins.saturating_sub(1);
            if timeout_spins == 0 {
                crate::return_errno!(EIO, "nvme command timed out");
            }
        }
    }
}
