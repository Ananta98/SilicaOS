use crate::errno::Result;
use crate::fs::vfs::{FileOps, NodeOps, OpenFlags};
use alloc::{boxed::Box, sync::Arc};

/// /dev/console - Standard kernel and user console stream.
pub struct ConsoleFile;

impl FileOps for ConsoleFile {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        // Reads from console not yet implemented
        Ok(0)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        if let Ok(s) = core::str::from_utf8(buf) {
            ostd::info!("{}", s.trim_end_matches('\n'));
        }
        Ok(buf.len())
    }
}

pub struct ConsoleNodeOps;

impl NodeOps for ConsoleNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(ConsoleFile))
    }
}

impl ConsoleFile {
    /// Helper to create a standard open File description for /dev/console.
    pub fn new_file() -> Arc<crate::fs::vfs::File> {
        let inode = Arc::new(crate::fs::vfs::INode::new(
            Box::new(ConsoleNodeOps),
            crate::fs::vfs::Mode::CHAR
                | crate::fs::vfs::Mode::RUSR
                | crate::fs::vfs::Mode::WUSR
                | crate::fs::vfs::Mode::WGRP
                | crate::fs::vfs::Mode::WOTH,
            2,
        ));
        Arc::new(crate::fs::vfs::File::new(
            Box::new(ConsoleFile),
            inode,
            crate::fs::vfs::OpenFlags::WRITE,
            false,
        ))
    }
}
