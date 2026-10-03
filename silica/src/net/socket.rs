// SPDX-License-Identifier: GPL-2.0

//! UNIX Socket Abstraction wrapping smoltcp Sockets.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU16, Ordering};
use ostd::task::Task;
use smoltcp::{
    iface::SocketHandle,
    socket::{tcp::Socket as TcpSocket, tcp::State as TcpState, udp::Socket as UdpSocket},
    wire::IpEndpoint,
};
use spin::Mutex;

use crate::{
    api::errno::{Errno, Result},
    net::interface::NET_MANAGER,
};

// Standard socket domains
pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const AF_INET6: i32 = 10;

// Standard socket types
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_RAW: i32 = 3;

// Standard protocols
pub const IPPROTO_IP: i32 = 0;
pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;

static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

/// Type of underlying socket implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketKind {
    Tcp(SocketHandle),
    Udp(SocketHandle),
}

/// In-kernel representation of an open network socket.
pub struct KernelSocket {
    pub domain: i32,
    pub sock_type: i32,
    pub protocol: i32,
    pub kind: Mutex<SocketKind>,
    pub nonblocking: Mutex<bool>,
    pub local_endpoint: Mutex<Option<IpEndpoint>>,
    pub remote_endpoint: Mutex<Option<IpEndpoint>>,
}

impl KernelSocket {
    /// Creates a new TCP socket.
    pub fn new_tcp(domain: i32) -> Result<Arc<Self>> {
        let handle = NET_MANAGER.lock().add_tcp_socket();
        Ok(Arc::new(Self {
            domain,
            sock_type: SOCK_STREAM,
            protocol: IPPROTO_TCP,
            kind: Mutex::new(SocketKind::Tcp(handle)),
            nonblocking: Mutex::new(false),
            local_endpoint: Mutex::new(None),
            remote_endpoint: Mutex::new(None),
        }))
    }

    /// Creates a new UDP socket.
    pub fn new_udp(domain: i32) -> Result<Arc<Self>> {
        let handle = NET_MANAGER.lock().add_udp_socket();
        Ok(Arc::new(Self {
            domain,
            sock_type: SOCK_DGRAM,
            protocol: IPPROTO_UDP,
            kind: Mutex::new(SocketKind::Udp(handle)),
            nonblocking: Mutex::new(false),
            local_endpoint: Mutex::new(None),
            remote_endpoint: Mutex::new(None),
        }))
    }

    /// Binds the socket to a local endpoint.
    pub fn bind(&self, endpoint: IpEndpoint) -> Result<()> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(_) => {
                *self.local_endpoint.lock() = Some(endpoint);
                Ok(())
            }
            SocketKind::Udp(handle) => {
                let mut net = NET_MANAGER.lock();
                let socket = net.get_sockets().get_mut::<UdpSocket>(handle);
                socket.bind(endpoint).map_err(|_| Errno::EADDRINUSE)?;
                *self.local_endpoint.lock() = Some(endpoint);
                Ok(())
            }
        }
    }

    /// Listens for incoming connections (TCP only).
    pub fn listen(&self, _backlog: i32) -> Result<()> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => {
                let local = self.local_endpoint.lock().ok_or(Errno::EDESTADDRREQ)?;
                let mut net = NET_MANAGER.lock();
                let socket = net.get_sockets().get_mut::<TcpSocket>(handle);
                socket.listen(local).map_err(|_| Errno::EADDRINUSE)?;
                Ok(())
            }
            SocketKind::Udp(_) => {
                crate::return_errno!(EOPNOTSUPP, "listen not supported on UDP socket")
            }
        }
    }

    /// Connects to a remote endpoint.
    pub fn connect(&self, endpoint: IpEndpoint) -> Result<()> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => {
                let local_port = self
                    .local_endpoint
                    .lock()
                    .map(|ep| ep.port)
                    .unwrap_or_else(|| NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed));

                let mut net = NET_MANAGER.lock();
                net.connect_tcp(handle, endpoint, local_port)?;
                *self.remote_endpoint.lock() = Some(endpoint);
                drop(net);

                // If blocking, wait until connection is established
                if !*self.nonblocking.lock() {
                    loop {
                        let mut net = NET_MANAGER.lock();
                        net.poll();
                        let socket = net.get_sockets().get_mut::<TcpSocket>(handle);
                        match socket.state() {
                            TcpState::Established => break,
                            TcpState::Closed | TcpState::TimeWait => {
                                return Err(Errno::ECONNREFUSED);
                            }
                            _ => {}
                        }
                        drop(net);
                        Task::yield_now();
                    }
                }
                Ok(())
            }
            SocketKind::Udp(_) => {
                *self.remote_endpoint.lock() = Some(endpoint);
                Ok(())
            }
        }
    }

    /// Accepts an incoming connection on a listening TCP socket.
    pub fn accept(&self) -> Result<(Arc<KernelSocket>, IpEndpoint)> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(listen_handle) => {
                let local = self.local_endpoint.lock().ok_or(Errno::EINVAL)?;
                loop {
                    let mut net = NET_MANAGER.lock();
                    net.poll();
                    let socket = net.get_sockets().get_mut::<TcpSocket>(listen_handle);

                    if socket.state() == TcpState::Established {
                        let remote = socket.remote_endpoint().ok_or(Errno::ENOTCONN)?;
                        // Allocate a new handle to continue listening
                        let new_listen_handle = net.add_tcp_socket();
                        let new_socket = net.get_sockets().get_mut::<TcpSocket>(new_listen_handle);
                        let _ = new_socket.listen(local);

                        // The current listen_handle is now the connected socket
                        let accepted_sock = Arc::new(KernelSocket {
                            domain: self.domain,
                            sock_type: SOCK_STREAM,
                            protocol: IPPROTO_TCP,
                            kind: Mutex::new(SocketKind::Tcp(listen_handle)),
                            nonblocking: Mutex::new(false),
                            local_endpoint: Mutex::new(Some(local)),
                            remote_endpoint: Mutex::new(Some(remote)),
                        });

                        // Swap listening socket to the new handle
                        *self.kind.lock() = SocketKind::Tcp(new_listen_handle);

                        return Ok((accepted_sock, remote));
                    }

                    if *self.nonblocking.lock() {
                        return Err(Errno::EWOULDBLOCK);
                    }

                    drop(net);
                    Task::yield_now();
                }
            }
            SocketKind::Udp(_) => crate::return_errno!(EOPNOTSUPP, "accept not supported on UDP socket"),
        }
    }

    /// Gets local socket address endpoint.
    pub fn getsockname(&self) -> Result<IpEndpoint> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => {
                let mut net = NET_MANAGER.lock();
                let socket = net.get_sockets().get_mut::<TcpSocket>(handle);
                if let Some(ep) = socket.local_endpoint() {
                    Ok(ep)
                } else if let Some(ep) = *self.local_endpoint.lock() {
                    Ok(ep)
                } else {
                    Err(Errno::ENOTCONN)
                }
            }
            SocketKind::Udp(_) => self.local_endpoint.lock().ok_or(Errno::ENOTCONN),
        }
    }

    /// Gets peer socket address endpoint.
    pub fn getpeername(&self) -> Result<IpEndpoint> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => {
                let mut net = NET_MANAGER.lock();
                let socket = net.get_sockets().get_mut::<TcpSocket>(handle);
                if let Some(ep) = socket.remote_endpoint() {
                    Ok(ep)
                } else if let Some(ep) = *self.remote_endpoint.lock() {
                    Ok(ep)
                } else {
                    Err(Errno::ENOTCONN)
                }
            }
            SocketKind::Udp(_) => self.remote_endpoint.lock().ok_or(Errno::ENOTCONN),
        }
    }

    /// Sends data through the socket.
    pub fn send(&self, data: &[u8], _flags: i32) -> Result<usize> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => loop {
                let mut net = NET_MANAGER.lock();
                net.poll();
                let socket = net.get_sockets().get_mut::<TcpSocket>(handle);

                if !socket.is_active() && socket.state() == TcpState::Closed {
                    return Err(Errno::EPIPE);
                }

                if socket.can_send() {
                    let sent = socket.send_slice(data).map_err(|_| Errno::ENOBUFS)?;
                    net.poll();
                    return Ok(sent);
                }

                if *self.nonblocking.lock() {
                    return Err(Errno::EWOULDBLOCK);
                }

                drop(net);
                Task::yield_now();
            },
            SocketKind::Udp(handle) => {
                let remote = self.remote_endpoint.lock().ok_or(Errno::EDESTADDRREQ)?;
                let mut net = NET_MANAGER.lock();
                let socket = net.get_sockets().get_mut::<UdpSocket>(handle);
                if socket.can_send() {
                    socket
                        .send_slice(data, remote)
                        .map_err(|_| Errno::ENOBUFS)?;
                    net.poll();
                    Ok(data.len())
                } else if *self.nonblocking.lock() {
                    Err(Errno::EWOULDBLOCK)
                } else {
                    Err(Errno::ENOBUFS)
                }
            }
        }
    }

    /// Receives data from the socket.
    pub fn recv(&self, buf: &mut [u8], _flags: i32) -> Result<usize> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => {
                loop {
                    let mut net = NET_MANAGER.lock();
                    net.poll();
                    let socket = net.get_sockets().get_mut::<TcpSocket>(handle);

                    if socket.can_recv() {
                        let n = socket.recv_slice(buf).map_err(|_| Errno::EIO)?;
                        return Ok(n);
                    }

                    match socket.state() {
                        TcpState::Closed | TcpState::CloseWait | TcpState::TimeWait => {
                            return Ok(0); // EOF
                        }
                        _ => {}
                    }

                    if *self.nonblocking.lock() {
                        return Err(Errno::EWOULDBLOCK);
                    }

                    drop(net);
                    Task::yield_now();
                }
            }
            SocketKind::Udp(handle) => loop {
                let mut net = NET_MANAGER.lock();
                net.poll();
                let socket = net.get_sockets().get_mut::<UdpSocket>(handle);

                if socket.can_recv() {
                    let (n, meta) = socket.recv_slice(buf).map_err(|_| Errno::EIO)?;
                    *self.remote_endpoint.lock() = Some(meta.endpoint);
                    return Ok(n);
                }

                if *self.nonblocking.lock() {
                    return Err(Errno::EWOULDBLOCK);
                }

                drop(net);
                Task::yield_now();
            },
        }
    }

    /// Sends a datagram to a specific destination endpoint.
    pub fn sendto(&self, data: &[u8], dest: Option<IpEndpoint>) -> Result<usize> {
        let target = dest
            .or(*self.remote_endpoint.lock())
            .ok_or(Errno::EDESTADDRREQ)?;
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Udp(handle) => {
                let mut net = NET_MANAGER.lock();
                let socket = net.get_sockets().get_mut::<UdpSocket>(handle);
                socket
                    .send_slice(data, target)
                    .map_err(|_| Errno::ENOBUFS)?;
                net.poll();
                Ok(data.len())
            }
            SocketKind::Tcp(_) => self.send(data, 0),
        }
    }

    /// Receives a datagram along with the sender endpoint.
    pub fn recvfrom(&self, buf: &mut [u8]) -> Result<(usize, Option<IpEndpoint>)> {
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Udp(handle) => loop {
                let mut net = NET_MANAGER.lock();
                net.poll();
                let socket = net.get_sockets().get_mut::<UdpSocket>(handle);

                if socket.can_recv() {
                    let (n, meta) = socket.recv_slice(buf).map_err(|_| Errno::EIO)?;
                    return Ok((n, Some(meta.endpoint)));
                }

                if *self.nonblocking.lock() {
                    return Err(Errno::EWOULDBLOCK);
                }

                drop(net);
                Task::yield_now();
            },
            SocketKind::Tcp(_) => {
                let n = self.recv(buf, 0)?;
                let peer = *self.remote_endpoint.lock();
                Ok((n, peer))
            }
        }
    }

    /// Shuts down all or part of a full-duplex connection.
    pub fn shutdown(&self, how: i32) -> Result<()> {
        let kind = *self.kind.lock();
        if let SocketKind::Tcp(handle) = kind {
            let mut net = NET_MANAGER.lock();
            let socket = net.get_sockets().get_mut::<TcpSocket>(handle);
            match how {
                0 | 1 | 2 => {
                    socket.close();
                    net.poll();
                    Ok(())
                }
                _ => crate::return_errno!(EINVAL, "invalid shutdown mode"),
            }
        } else {
            Ok(())
        }
    }

    /// Closes the socket and removes it from the global socket set.
    pub fn close(&self) {
        let mut net = NET_MANAGER.lock();
        let kind = *self.kind.lock();
        match kind {
            SocketKind::Tcp(handle) => {
                let socket = net.get_sockets().get_mut::<TcpSocket>(handle);
                socket.abort();
                net.remove_socket(handle);
            }
            SocketKind::Udp(handle) => {
                let socket = net.get_sockets().get_mut::<UdpSocket>(handle);
                socket.close();
                net.remove_socket(handle);
            }
        }
    }
}

impl Drop for KernelSocket {
    fn drop(&mut self) {
        self.close();
    }
}
