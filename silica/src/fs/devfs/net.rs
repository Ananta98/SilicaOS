// SPDX-License-Identifier: GPL-2.0

//! Device Filesystem (`/dev`) Network Device Node Adapter.
//!
//! Exposes network devices (e.g. `/dev/eth0`) as character-like devices in devfs.
//! Reading from `/dev/eth0` receives raw Ethernet frames.
//! Writing to `/dev/eth0` transmits raw Ethernet frames.

use alloc::boxed::Box;
use alloc::sync::Arc;
use ostd::mm::FallibleVmWrite;

use crate::{
    api::errno::{Errno, Result},
    drivers::net::NetDevice,
    fs::vfs::{FileOps, INodeAttr, Mode, NodeOps, OpenFlags},
};

/// Generic network device adapter node operations in devfs.
pub struct GenericNetNodeOps {
    dev: Arc<dyn NetDevice>,
}

impl GenericNetNodeOps {
    pub fn new(dev: Arc<dyn NetDevice>) -> Self {
        Self { dev }
    }
}

impl NodeOps for GenericNetNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(GenericNetFileOps {
            dev: Arc::clone(&self.dev),
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: 0,
            mode: Mode::CHAR
                | Mode::RUSR
                | Mode::WUSR
                | Mode::RGRP
                | Mode::ROTH,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

/// File operations for network devices accessed via /dev (e.g. /dev/eth0).
pub struct GenericNetFileOps {
    dev: Arc<dyn NetDevice>,
}

impl FileOps for GenericNetFileOps {
    fn read(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        // Read raw packet from interface
        self.dev.receive(buf)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        // Transmit raw packet through interface
        self.dev.transmit(buf)?;
        Ok(buf.len())
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> Result<usize> {
        match cmd {
            // SIOCGIFHWADDR (0x8927): Get Hardware / MAC Address
            0x8927 => {
                if arg == 0 {
                    return Err(Errno::EFAULT);
                }
                let mac = self.dev.mac_address();
                let proc = crate::proc::thread::Thread::current_proc().ok_or(Errno::ESRCH)?;
                let vmar = proc.vmspace();
                let mut writer = vmar.vm_space().writer(arg, 6).map_err(|_| Errno::EFAULT)?;
                let mut reader = ostd::mm::VmReader::from(&mac[..]);
                writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
                Ok(0)
            }
            _ => crate::return_errno!(ENOTTY, "unsupported netdev ioctl"),
        }
    }
}
