// SPDX-License-Identifier: GPL-2.0

//! Device Filesystem (`/dev`).

pub mod devices;

pub use devices::{ConsoleFile, NullFile, ZeroFile};

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::errno::Result;
use crate::fs::vfs::{FileSystem, INode, Mode, NodeOps};
use devices::{ConsoleNodeOps, NullNodeOps, ZeroNodeOps};

pub struct DevFsDirOps;

impl NodeOps for DevFsDirOps {
    fn lookup(&self, name: &str) -> Result<Arc<INode>> {
        match name {
            "null" => Ok(Arc::new(INode::new(
                Box::new(NullNodeOps),
                Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
                1,
            ))),
            "zero" => Ok(Arc::new(INode::new(
                Box::new(ZeroNodeOps),
                Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
                2,
            ))),
            "console" => Ok(Arc::new(INode::new(
                Box::new(ConsoleNodeOps),
                Mode::CHAR | Mode::RUSR | Mode::WUSR | Mode::WGRP | Mode::WOTH,
                3,
            ))),
            _ => crate::return_errno!(ENOENT, "device not found"),
        }
    }
}

pub struct DevFs;

impl DevFs {
    pub fn new() -> Self {
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
            Mode::DIR | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            0,
        )))
    }
}
