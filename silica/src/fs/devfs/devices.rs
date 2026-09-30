// SPDX-License-Identifier: GPL-2.0

//! Device nodes for DevFS (/dev/null, /dev/zero, /dev/console).

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::errno::Result;
use crate::fs::vfs::{File, FileOps, INode, Mode, NodeOps, OpenFlags};

/// /dev/null - Discards all writes, reads always return EOF (0 bytes).
pub struct NullFile;

impl FileOps for NullFile {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}

pub struct NullNodeOps;

impl NodeOps for NullNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(NullFile))
    }
}

impl NullFile {
    /// Helper to create a standard open File description for /dev/null.
    pub fn new_file() -> Arc<File> {
        let inode = Arc::new(INode::new(
            Box::new(NullNodeOps),
            Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
            1,
        ));
        Arc::new(File::new(
            Box::new(NullFile),
            inode,
            OpenFlags::READ | OpenFlags::WRITE,
            false,
        ))
    }
}

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
    pub fn new_file() -> Arc<File> {
        let inode = Arc::new(INode::new(
            Box::new(ConsoleNodeOps),
            Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::WGRP | Mode::WOTH,
            2,
        ));
        Arc::new(File::new(
            Box::new(ConsoleFile),
            inode,
            OpenFlags::WRITE,
            false,
        ))
    }
}
