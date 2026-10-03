use alloc::boxed::Box;

use crate::api::errno::Result;
use crate::fs::vfs::{FileOps, NodeOps, OpenFlags};

/// /dev/zero - Discards all writes, reads return infinite zero-bytes.
pub struct ZeroFile;

impl FileOps for ZeroFile {
    fn read(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}

pub struct ZeroNodeOps;

impl NodeOps for ZeroNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(ZeroFile))
    }
}
