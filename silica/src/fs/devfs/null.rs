use alloc::boxed::Box;
use crate::{
    api::errno::Result,
    fs::vfs::{FileOps, INodeAttr, Mode, NodeOps, OpenFlags},
};

pub struct NullNodeOps;
impl NodeOps for NullNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(NullFile))
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

pub struct NullFile;
impl FileOps for NullFile {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        Ok(0) // EOF
    }
    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        Ok(buf.len()) // Discard all
    }
}

impl NullFile {
    /// Helper to create a standard open File description for /dev/null.
    pub fn new_file() -> alloc::sync::Arc<crate::fs::vfs::File> {
        let inode = alloc::sync::Arc::new(crate::fs::vfs::INode::new(
            Box::new(NullNodeOps),
            crate::fs::vfs::Mode::CHAR | crate::fs::vfs::Mode::RUSR | crate::fs::vfs::Mode::WUSR | crate::fs::vfs::Mode::RGRP | crate::fs::vfs::Mode::WGRP | crate::fs::vfs::Mode::ROTH | crate::fs::vfs::Mode::WOTH,
            1,
        ));
        alloc::sync::Arc::new(crate::fs::vfs::File::new(
            Box::new(NullFile),
            inode,
            crate::fs::vfs::OpenFlags::READ | crate::fs::vfs::OpenFlags::WRITE,
            false,
        ))
    }
}
