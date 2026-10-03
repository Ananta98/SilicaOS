// SPDX-License-Identifier: GPL-2.0

//! Network Device Driver Abstractions.
//!
//! Provides the core [`NetDevice`] trait, dynamic registration/unregistration for kernel modules,
//! and network device drivers such as `e1000`.

pub mod e1000;

use alloc::{string::String, sync::Arc, vec::Vec};
use crate::api::errno::Result;

/// A network interface device (e.g. Ethernet controller).
pub trait NetDevice: Send + Sync {
    /// The unique interface name (e.g. "eth0").
    fn name(&self) -> String;

    /// MAC address of the network interface.
    fn mac_address(&self) -> [u8; 6];

    /// Whether the device is currently capable of transmitting a packet.
    fn can_transmit(&self) -> bool;

    /// Transmit an Ethernet frame.
    fn transmit(&self, packet: &[u8]) -> Result<()>;

    /// Receive an Ethernet frame into the buffer, returning the number of bytes read.
    /// Returns Ok(0) if no packet is currently available.
    fn receive(&self, buf: &mut [u8]) -> Result<usize>;

    /// Maximum Transmission Unit (MTU) in bytes (usually 1500 for Ethernet).
    fn mtu(&self) -> usize {
        1500
    }
}

/// Registers a network device into the global network driver layer and devfs.
pub fn register_network_device(device: Arc<dyn NetDevice>) {
    ostd::info!(
        "Net: Registered network device \"/dev/{}\" (MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, MTU: {})",
        device.name(),
        device.mac_address()[0],
        device.mac_address()[1],
        device.mac_address()[2],
        device.mac_address()[3],
        device.mac_address()[4],
        device.mac_address()[5],
        device.mtu(),
    );
    crate::drivers::register_netdev(device.clone());
    crate::net::attach_device(device);
}

/// Unregisters a network device by name.
pub fn unregister_network_device(name: &str) {
    crate::drivers::unregister_netdev(name);
    crate::net::detach_device(name);
}

/// Retrieves a network device by name.
pub fn get_network_device(name: &str) -> Option<Arc<dyn NetDevice>> {
    crate::drivers::get_netdevs().into_iter().find(|d| d.name() == name)
}

/// Lists all registered network device names.
pub fn list_network_devices() -> Vec<String> {
    crate::drivers::get_netdevs().into_iter().map(|d| d.name()).collect()
}
