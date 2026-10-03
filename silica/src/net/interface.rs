// SPDX-License-Identifier: GPL-2.0

//! Network Interface Management using smoltcp.
//!
//! Maintains the primary network [`smoltcp::iface::Interface`] and [`smoltcp::iface::SocketSet`].

use alloc::{sync::Arc, vec::Vec};
use ostd::timer::{Jiffies, TIMER_FREQ};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    socket::{
        tcp::{Socket as TcpSocket, SocketBuffer as TcpSocketBuffer},
        udp::{PacketBuffer as UdpPacketBuffer, PacketMetadata as UdpPacketMetadata, Socket as UdpSocket},
    },
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr},
};
use spin::Mutex;

use super::device::SmolNetDevice;
use crate::{
    api::errno::{Errno, Result},
    drivers::net::NetDevice,
};

/// Returns current timestamp as a [`smoltcp::time::Instant`].
pub fn current_instant() -> Instant {
    let freq = if TIMER_FREQ == 0 { 100 } else { TIMER_FREQ };
    let millis = (Jiffies::elapsed().as_u64() * 1000) / freq;
    Instant::from_millis(millis as i64)
}

/// Active interface with bound device.
pub struct BoundInterface {
    pub iface: Interface,
    pub device: SmolNetDevice,
}

/// Global network state managing interface polling and active sockets.
pub struct NetworkManager {
    pub bound: Option<BoundInterface>,
    pub sockets: Option<SocketSet<'static>>,
}

impl NetworkManager {
    pub const fn new() -> Self {
        Self {
            bound: None,
            sockets: None,
        }
    }

    /// Access or lazily initialize the socket set.
    pub fn get_sockets(&mut self) -> &mut SocketSet<'static> {
        self.sockets.get_or_insert_with(|| SocketSet::new(Vec::new()))
    }

    /// Connect a TCP socket to an endpoint using the bound interface context.
    pub fn connect_tcp(
        &mut self,
        handle: SocketHandle,
        endpoint: IpEndpoint,
        local_port: u16,
    ) -> Result<()> {
        let bound = self.bound.as_mut().ok_or(Errno::ENETDOWN)?;
        let sockets = self.sockets.get_or_insert_with(|| SocketSet::new(Vec::new()));
        let socket = sockets.get_mut::<TcpSocket>(handle);
        socket
            .connect(bound.iface.context(), endpoint, local_port)
            .map_err(|_| Errno::ECONNREFUSED)
    }

    /// Attach a network device and configure standard IPv4 & IPv6 default networking.
    pub fn attach_device(&mut self, dev: Arc<dyn NetDevice>) {
        let mac = dev.mac_address();
        let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
        let mut smol_dev = SmolNetDevice::new(dev);

        let mut config = Config::new(hw_addr);
        config.random_seed = 0x5111CA05;

        let now = current_instant();
        let mut iface = Interface::new(config, &mut smol_dev, now);

        // Configure standard QEMU / default IPv4: 10.0.2.15/24, Gateway: 10.0.2.2
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)));
            // Configure IPv6 link-local address: fe80::5111:ca05:1/64
            let _ = addrs.push(IpCidr::Ipv6(Ipv6Cidr::new(
                Ipv6Address::new(0xfe80, 0, 0, 0, 0x5111, 0xca05, 0, 1),
                64,
            )));
        });

        // Set default IPv4 gateway
        iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2)).ok();

        ostd::info!(
            "Net: Configured smoltcp interface on {} (IPv4: 10.0.2.15/24, IPv6: fe80::5111:ca05:1/64)",
            smol_dev.net_dev().name()
        );

        self.bound = Some(BoundInterface {
            iface,
            device: smol_dev,
        });
    }

    /// Detach a device if matching the name.
    pub fn detach_device(&mut self, name: &str) {
        if let Some(ref bound) = self.bound {
            if bound.device.net_dev().name() == name {
                self.bound = None;
                ostd::info!("Net: Detached device {}", name);
            }
        }
    }

    /// Polls the network interface, processing incoming/outgoing packets.
    pub fn poll(&mut self) -> bool {
        let now = current_instant();
        let sockets = self.sockets.get_or_insert_with(|| SocketSet::new(Vec::new()));
        if let Some(ref mut bound) = self.bound {
            let poll_res = bound.iface.poll(now, &mut bound.device, sockets);
            poll_res != smoltcp::iface::PollResult::None
        } else {
            false
        }
    }

    /// Creates and adds a TCP socket, returning its handle.
    pub fn add_tcp_socket(&mut self) -> SocketHandle {
        let rx_buffer = TcpSocketBuffer::new(alloc::vec![0u8; 65536]);
        let tx_buffer = TcpSocketBuffer::new(alloc::vec![0u8; 65536]);
        let tcp_socket = TcpSocket::new(rx_buffer, tx_buffer);
        self.get_sockets().add(tcp_socket)
    }

    /// Creates and adds a UDP socket, returning its handle.
    pub fn add_udp_socket(&mut self) -> SocketHandle {
        let rx_meta = alloc::vec![UdpPacketMetadata::EMPTY; 32];
        let rx_payload = alloc::vec![0u8; 65536];
        let rx_buffer = UdpPacketBuffer::new(rx_meta, rx_payload);

        let tx_meta = alloc::vec![UdpPacketMetadata::EMPTY; 32];
        let tx_payload = alloc::vec![0u8; 65536];
        let tx_buffer = UdpPacketBuffer::new(tx_meta, tx_payload);

        let udp_socket = UdpSocket::new(rx_buffer, tx_buffer);
        self.get_sockets().add(udp_socket)
    }

    /// Removes a socket from the set.
    pub fn remove_socket(&mut self, handle: SocketHandle) {
        self.get_sockets().remove(handle);
    }
}

pub static NET_MANAGER: Mutex<NetworkManager> = Mutex::new(NetworkManager::new());

/// Top-level helper to poll the network stack.
pub fn poll() -> bool {
    NET_MANAGER.lock().poll()
}
