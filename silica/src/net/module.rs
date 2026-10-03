// SPDX-License-Identifier: GPL-2.0

//! Kernel Module Network Device Registration Interface.

use alloc::sync::Arc;
use crate::drivers::net::NetDevice;

/// Registers a network device into the kernel network subsystem and devfs.
pub fn register_netdev(device: Arc<dyn NetDevice>) {
    crate::drivers::net::register_network_device(device);
}

/// Unregisters a network device by name from the kernel network subsystem.
pub fn unregister_netdev(name: &str) {
    crate::drivers::net::unregister_network_device(name);
}
