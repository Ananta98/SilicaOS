// SPDX-License-Identifier: GPL-2.0

//! Device Filesystem (`/dev`).
//!
//! Provides a dynamic, extensible character and block device node registry
//! for SilicaOS drivers to register and unregister device nodes at runtime.

pub mod blk;
pub mod chr;
pub mod console;
pub mod null;
pub mod zero;

pub use console::ConsoleFile;
pub use null::NullFile;
pub use zero::ZeroFile;

use crate::{
    errno::Result,
    fs::vfs::{FileOps, FileSystem, INode, INodeAttr, Mode, NodeOps, OpenFlags},
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use blk::GenericBlkNodeOps;
use chr::GenericChrNodeOps;
use console::ConsoleNodeOps;
use null::NullNodeOps;
use zero::ZeroNodeOps;

/// Operations on the `/dev` root directory inode.
pub struct DevFsDirOps;

impl NodeOps for DevFsDirOps {
    fn lookup(&self, name: &str) -> Result<Arc<INode>> {
        // 1. Check static dev nodes first
        match name {
            "null" => {
                return Ok(Arc::new(INode::new(
                    Box::new(NullNodeOps),
                    Mode::CHAR
                        | Mode::RUSR
                        | Mode::WUSR
                        | Mode::RGRP
                        | Mode::WGRP
                        | Mode::ROTH
                        | Mode::WOTH,
                    1,
                )));
            }
            "zero" => {
                return Ok(Arc::new(INode::new(
                    Box::new(ZeroNodeOps),
                    Mode::CHAR
                        | Mode::RUSR
                        | Mode::WUSR
                        | Mode::RGRP
                        | Mode::WGRP
                        | Mode::ROTH
                        | Mode::WOTH,
                    2,
                )));
            }
            "console" => {
                return Ok(Arc::new(INode::new(
                    Box::new(ConsoleNodeOps),
                    Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::WGRP | Mode::WOTH,
                    3,
                )));
            }
            _ => {}
        }

        // 2. Check character devices
        for dev in crate::drivers::get_chrdevs() {
            if dev.name() == name {
                return Ok(Arc::new(INode::new(
                    Box::new(GenericChrNodeOps::new(Arc::clone(&dev))),
                    Mode::CHAR
                        | Mode::RUSR
                        | Mode::WUSR
                        | Mode::RGRP
                        | Mode::WGRP
                        | Mode::ROTH
                        | Mode::WOTH,
                    100, // In real implementations, you'd use a unique inode number
                )));
            }
        }

        // 3. Check block devices
        for dev in crate::drivers::get_blkdevs() {
            if dev.name() == name {
                return Ok(Arc::new(INode::new(
                    Box::new(GenericBlkNodeOps::new(Arc::clone(&dev))),
                    Mode::BLOCK | Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP,
                    100, // In real implementations, you'd use a unique inode number
                )));
            }
        }

        crate::return_errno!(ENOENT, "device not found")
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        if flags.contains(OpenFlags::WRITE) {
            crate::return_errno!(EPERM, "cannot open /dev directory for writing");
        }
        Ok(Box::new(DevFsDirFileOps))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: 0,
            mode: Mode::DIR
                | Mode::RUSR
                | Mode::WUSR
                | Mode::XUSR
                | Mode::RGRP
                | Mode::XGRP
                | Mode::ROTH
                | Mode::XOTH,
            nlink: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

/// Directory file operations for reading `/dev`.
pub struct DevFsDirFileOps;

impl FileOps for DevFsDirFileOps {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot read raw bytes from directory");
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot write to directory");
    }

    fn readdir(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(b".\n..\n");
        out.extend_from_slice(b"null\n");
        out.extend_from_slice(b"zero\n");
        out.extend_from_slice(b"console\n");

        for dev in crate::drivers::get_chrdevs() {
            out.extend_from_slice(dev.name().as_bytes());
            out.push(b'\n');
        }

        for dev in crate::drivers::get_blkdevs() {
            out.extend_from_slice(dev.name().as_bytes());
            out.push(b'\n');
        }

        Ok(out)
    }
}

/// The DevFS filesystem representation.
pub struct DevFs;

impl DevFs {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DevFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for DevFs {
    fn name(&self) -> &'static str {
        "devfs"
    }

    fn root(&self) -> Result<Arc<INode>> {
        Ok(Arc::new(INode::new(
            Box::new(DevFsDirOps),
            Mode::DIR
                | Mode::RUSR
                | Mode::WUSR
                | Mode::XUSR
                | Mode::RGRP
                | Mode::XGRP
                | Mode::ROTH
                | Mode::XOTH,
            0,
        )))
    }
}
