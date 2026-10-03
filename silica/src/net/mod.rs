// SPDX-License-Identifier: GPL-2.0

//! Network Subsystem for SilicaOS using smoltcp.

pub mod device;
pub mod interface;
pub mod module;
pub mod socket;

use crate::api::errno::Result;
use crate::drivers::net::NetDevice;
use alloc::sync::Arc;

pub use interface::{NET_MANAGER, poll};
pub use module::{register_netdev, unregister_netdev};

/// Attaches a network device to the global network stack interface.
pub fn attach_device(dev: Arc<dyn NetDevice>) {
    NET_MANAGER.lock().attach_device(dev);
}

/// Detaches a network device by name from the global network stack interface.
pub fn detach_device(name: &str) {
    NET_MANAGER.lock().detach_device(name);
}

/// Initializes the network subsystem.
pub fn init() -> Result<()> {
    ostd::info!("Net: Network subsystem initialized");
    Ok(())
}

crate::module!(
    "SilicaOS Network Subsystem",
    "SilicaOS Team",
    crate::modules::InitcallLevel::Core,
    init
);
