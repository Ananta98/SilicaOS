// SPDX-License-Identifier: GPL-2.0

//! NVMe Active Namespace representing a block storage device.

use alloc::{string::String, sync::Arc};
use spin::Mutex;

use ostd::{
    io::IoMem,
    mm::{
        HasPaddr, VmReader, VmWriter,
        dma::DmaCoherent,
        io::util::HasVmReaderWriter,
    },
};

use super::{
    commands::{NvmeSqe, opcodes},
    queue::NvmeQueuePair,
};
use crate::{
    drivers::{Device, DeviceType, block::BlockDevice},
    errno::{Errno, Result},
};

/// Represents an active NVMe Namespace that provides block-level I/O.
pub struct NvmeNamespace {
    pub nsid: u32,
    pub name: String,
    pub block_size: usize,
    pub block_count: u64,
    pub io_queue: Arc<Mutex<NvmeQueuePair>>,
    pub mmio: IoMem,
}

impl Device for NvmeNamespace {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}

impl BlockDevice for NvmeNamespace {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&self, start_block: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() % self.block_size != 0 {
            crate::return_errno!(EINVAL, "buffer length must be a multiple of block size");
        }

        let num_blocks = (buf.len() / self.block_size) as u16;
        let dma_frames = (buf.len() + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let dma = DmaCoherent::alloc(dma_frames, true).map_err(|_| Errno::ENOMEM)?;

        let prp1 = dma.paddr() as u64;
        let sqe = NvmeSqe::read(self.nsid, start_block, num_blocks, prp1, 0, 0);

        let mut queue = self.io_queue.lock();
        queue.submit_and_wait(sqe, &self.mmio)?;

        // Copy read bytes from DMA buffer into caller's buffer safely
        let mut reader = dma.reader();
        let mut writer = VmWriter::from(buf);
        let bytes_copied = reader.read(&mut writer);

        Ok(bytes_copied)
    }

    fn write_blocks(&self, start_block: u64, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() % self.block_size != 0 {
            crate::return_errno!(EINVAL, "buffer length must be a multiple of block size");
        }

        let num_blocks = (buf.len() / self.block_size) as u16;
        let dma_frames = (buf.len() + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let dma = DmaCoherent::alloc(dma_frames, true).map_err(|_| Errno::ENOMEM)?;

        // Copy bytes to write into DMA buffer safely
        let mut reader = VmReader::from(buf);
        let mut writer = dma.writer();
        let _ = reader.read(&mut writer);

        let prp1 = dma.paddr() as u64;
        let sqe = NvmeSqe::write(self.nsid, start_block, num_blocks, prp1, 0, 0);

        let mut queue = self.io_queue.lock();
        queue.submit_and_wait(sqe, &self.mmio)?;

        Ok(buf.len())
    }

    fn flush(&self) -> Result<()> {
        let mut sqe = NvmeSqe::default();
        sqe.cdw0 = opcodes::FLUSH as u32;
        sqe.nsid = self.nsid;

        let mut queue = self.io_queue.lock();
        queue.submit_and_wait(sqe, &self.mmio)?;
        Ok(())
    }
}
