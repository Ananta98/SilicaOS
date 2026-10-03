// SPDX-License-Identifier: GPL-2.0

//! smoltcp Device Trait Adapter for Kernel Network Devices.
//!
//! Bridges [`crate::drivers::net::NetDevice`] to [`smoltcp::phy::Device`].

use alloc::{sync::Arc, vec, vec::Vec};
use smoltcp::{
    phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};

use crate::drivers::net::NetDevice;

/// Adapter implementing `smoltcp::phy::Device` for an `Arc<dyn NetDevice>`.
pub struct SmolNetDevice {
    dev: Arc<dyn NetDevice>,
}

impl SmolNetDevice {
    pub fn new(dev: Arc<dyn NetDevice>) -> Self {
        Self { dev }
    }

    pub fn net_dev(&self) -> &Arc<dyn NetDevice> {
        &self.dev
    }
}

pub struct SmolRxToken {
    buffer: Vec<u8>,
}

impl RxToken for SmolRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}


pub struct SmolTxToken {
    dev: Arc<dyn NetDevice>,
}

impl TxToken for SmolTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let res = f(&mut buffer);
        let _ = self.dev.transmit(&buffer);
        res
    }
}

impl Device for SmolNetDevice {
    type RxToken<'a> = SmolRxToken where Self: 'a;
    type TxToken<'a> = SmolTxToken where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut buf = vec![0u8; self.dev.mtu() + 18];
        match self.dev.receive(&mut buf) {
            Ok(n) if n > 0 => {
                buf.truncate(n);
                let rx = SmolRxToken { buffer: buf };
                let tx = SmolTxToken {
                    dev: Arc::clone(&self.dev),
                };
                Some((rx, tx))
            }
            _ => None,
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.dev.can_transmit() {
            Some(SmolTxToken {
                dev: Arc::clone(&self.dev),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = self.dev.mtu() + 14;
        caps.medium = Medium::Ethernet;
        caps.checksum = ChecksumCapabilities::default();
        caps
    }
}
