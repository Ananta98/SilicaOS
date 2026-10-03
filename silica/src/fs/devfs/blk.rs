use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::{
    drivers::BlockDevice,
    api::errno::Result,
    fs::vfs::{FileOps, INodeAttr, Mode, NodeOps, OpenFlags},
};

/// Generic block device adapter node operations.
pub struct GenericBlkNodeOps {
    dev: Arc<dyn BlockDevice>,
}

impl GenericBlkNodeOps {
    pub fn new(dev: Arc<dyn BlockDevice>) -> Self {
        Self { dev }
    }
}

impl NodeOps for GenericBlkNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(GenericBlkFileOps {
            dev: Arc::clone(&self.dev),
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: self.dev.capacity() as usize,
            mode: Mode::BLOCK | Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

/// File operations for block devices accessed via /dev.
pub struct GenericBlkFileOps {
    dev: Arc<dyn BlockDevice>,
}

impl FileOps for GenericBlkFileOps {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let block_size = self.dev.block_size() as u64;
        let start_block = offset / block_size;
        let block_offset = (offset % block_size) as usize;

        if block_offset == 0 && buf.len() % (block_size as usize) == 0 {
            return self.dev.read_blocks(start_block, buf);
        }

        // Handle unaligned read via bounce buffer
        let total_blocks = ((block_offset + buf.len() + (block_size as usize) - 1)
            / (block_size as usize)) as u64;
        let mut bounce = alloc::vec![0u8; (total_blocks * block_size) as usize];
        self.dev.read_blocks(start_block, &mut bounce)?;
        let to_copy = buf.len().min(bounce.len().saturating_sub(block_offset));
        buf[..to_copy].copy_from_slice(&bounce[block_offset..block_offset + to_copy]);
        Ok(to_copy)
    }

    fn write(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        let block_size = self.dev.block_size() as u64;
        let start_block = offset / block_size;
        let block_offset = (offset % block_size) as usize;

        if block_offset == 0 && buf.len() % (block_size as usize) == 0 {
            return self.dev.write_blocks(start_block, buf);
        }

        let total_blocks = ((block_offset + buf.len() + (block_size as usize) - 1)
            / (block_size as usize)) as u64;
        let mut bounce = alloc::vec![0u8; (total_blocks * block_size) as usize];
        self.dev.read_blocks(start_block, &mut bounce)?;
        let to_copy = buf.len().min(bounce.len().saturating_sub(block_offset));
        bounce[block_offset..block_offset + to_copy].copy_from_slice(&buf[..to_copy]);
        self.dev.write_blocks(start_block, &bounce)?;
        Ok(to_copy)
    }

    fn close(&self) -> Result<()> {
        let _ = self.dev.flush();
        Ok(())
    }
}
