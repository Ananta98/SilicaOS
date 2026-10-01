use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::{
    drivers::CharacterDevice,
    errno::Result,
    fs::vfs::{FileOps, INodeAttr, Mode, NodeOps, OpenFlags},
};

/// Generic character device adapter node operations.
pub struct GenericChrNodeOps {
    dev: Arc<dyn CharacterDevice>,
}

impl GenericChrNodeOps {
    pub fn new(dev: Arc<dyn CharacterDevice>) -> Self {
        Self { dev }
    }
}

impl NodeOps for GenericChrNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(GenericChrFileOps {
            dev: Arc::clone(&self.dev),
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: 0,
            mode: Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

/// File operations for character devices accessed via /dev.
pub struct GenericChrFileOps {
    dev: Arc<dyn CharacterDevice>,
}

impl FileOps for GenericChrFileOps {
    fn read(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.dev.read(buf)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        self.dev.write(buf)
    }
}
