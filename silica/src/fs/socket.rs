// SPDX-License-Identifier: GPL-2.0

//! Socket VFS Adapter and File Operations.
//!
//! Provides [`SocketFileOps`] implementing [`FileOps`] so that user space can
//! perform `read(2)`, `write(2)`, `close(2)`, and `poll(2)` on network socket descriptors.

use alloc::{boxed::Box, sync::Arc};
use smoltcp::socket::{
    tcp::{Socket as TcpSocket, State as TcpState},
    udp::Socket as UdpSocket,
};

use crate::{
    api::errno::Result,
    fs::{
        poll::PollEvents,
        vfs::{File, FileOps, INode, INodeAttr, Mode, NodeOps, OpenFlags},
    },
    net::{
        interface::NET_MANAGER,
        socket::{KernelSocket, SocketKind},
    },
};

/// Socket file operations for `read`, `write`, `close`, and `poll`.
pub struct SocketFileOps {
    pub socket: Arc<KernelSocket>,
}

impl FileOps for SocketFileOps {
    fn read(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.socket.recv(buf, 0)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        self.socket.send(buf, 0)
    }

    fn close(&self) -> Result<()> {
        self.socket.close();
        Ok(())
    }

    fn as_socket(&self) -> Option<Arc<KernelSocket>> {
        Some(self.socket.clone())
    }

    fn poll(&self) -> PollEvents {
        let mut events = PollEvents::empty();
        let mut net = NET_MANAGER.lock();
        net.poll();

        match *self.socket.kind.lock() {
            SocketKind::Tcp(handle) => {
                let s = net.get_sockets().get_mut::<TcpSocket>(handle);
                if s.can_recv() {
                    events |= PollEvents::IN | PollEvents::RDNORM;
                }
                if s.can_send() {
                    events |= PollEvents::OUT | PollEvents::WRNORM;
                }
                if s.state() == TcpState::Closed || s.state() == TcpState::CloseWait {
                    events |= PollEvents::HUP;
                }
            }
            SocketKind::Udp(handle) => {
                let s = net.get_sockets().get_mut::<UdpSocket>(handle);
                if s.can_recv() {
                    events |= PollEvents::IN | PollEvents::RDNORM;
                }
                if s.can_send() {
                    events |= PollEvents::OUT | PollEvents::WRNORM;
                }
            }
        }

        events
    }
}

pub struct SocketNodeOps;

impl NodeOps for SocketNodeOps {
    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        crate::return_errno!(EOPNOTSUPP, "cannot directly open socket inode")
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: 0,
            mode: Mode::SOCKET | Mode::RUSR | Mode::WUSR,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

/// Helper to wrap a [`KernelSocket`] in an open [`File`].
pub fn create_socket_file(socket: Arc<KernelSocket>) -> Arc<File> {
    let inode = Arc::new(INode::new(
        Box::new(SocketNodeOps),
        Mode::SOCKET | Mode::RUSR | Mode::WUSR,
        900,
    ));
    Arc::new(File::new(
        Box::new(SocketFileOps { socket }),
        inode,
        OpenFlags::READ | OpenFlags::WRITE,
        false, // Sockets are not seekable streams
    ))
}
