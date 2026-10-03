// SPDX-License-Identifier: GPL-2.0

//! UNIX Network System Calls implementation.
//!
//! Provides Linux-compatible network system calls:
//! - `socket(2)`
//! - `bind(2)`
//! - `connect(2)`
//! - `listen(2)`
//! - `accept(2)`
//! - `sendto(2)`
//! - `recvfrom(2)`
//! - `shutdown(2)`
//! - `getsockname(2)`
//! - `getpeername(2)`
//! - `setsockopt(2)`
//! - `getsockopt(2)`

use alloc::sync::Arc;
use ostd::{
    arch::cpu::context::UserContext,
    mm::io::{FallibleVmRead, FallibleVmWrite},
};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address};

use crate::{
    api::errno::{Errno, Result},
    fs::{fd::FdFlags, socket::create_socket_file},
    net::socket::{
        KernelSocket, AF_INET, AF_INET6, IPPROTO_TCP, IPPROTO_UDP, SOCK_DGRAM, SOCK_STREAM,
    },
    proc::{thread::Thread, Proc},
};

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,       // big-endian
    pub sin_addr: [u8; 4],
    pub sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SockAddrIn6 {
    pub sin6_family: u16,
    pub sin6_port: u16,      // big-endian
    pub sin6_flowinfo: u32,
    pub sin6_addr: [u8; 16],
    pub sin6_scope_id: u32,
}

fn get_socket(fd: i32) -> Result<Arc<KernelSocket>> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let file = proc.fd_table.lock().get(fd)?;
    file.as_socket().ok_or(Errno::ENOTSOCK)
}

fn read_sockaddr(proc: &Arc<Proc>, addr_ptr: usize, addrlen: u32) -> Result<IpEndpoint> {
    if addr_ptr == 0 || addrlen < 2 {
        return Err(Errno::EINVAL);
    }
    let vmar = proc.vmspace();

    let mut family_bytes = [0u8; 2];
    let mut reader = vmar
        .vm_space()
        .reader(addr_ptr, 2)
        .map_err(|_| Errno::EFAULT)?;
    let mut writer = ostd::mm::VmWriter::from(&mut family_bytes[..]);
    reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;
    let family = u16::from_ne_bytes(family_bytes) as i32;

    match family {
        AF_INET => {
            if addrlen < 16 {
                return Err(Errno::EINVAL);
            }
            let mut buf = [0u8; 16];
            let mut reader = vmar
                .vm_space()
                .reader(addr_ptr, 16)
                .map_err(|_| Errno::EFAULT)?;
            let mut writer = ostd::mm::VmWriter::from(&mut buf[..]);
            reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;

            let port = u16::from_be_bytes([buf[2], buf[3]]);
            let ip = Ipv4Address::from_octets([buf[4], buf[5], buf[6], buf[7]]);
            Ok(IpEndpoint::new(IpAddress::Ipv4(ip), port))
        }
        AF_INET6 => {
            if addrlen < 28 {
                return Err(Errno::EINVAL);
            }
            let mut buf = [0u8; 28];
            let mut reader = vmar
                .vm_space()
                .reader(addr_ptr, 28)
                .map_err(|_| Errno::EFAULT)?;
            let mut writer = ostd::mm::VmWriter::from(&mut buf[..]);
            reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;

            let port = u16::from_be_bytes([buf[2], buf[3]]);
            let mut ip_bytes = [0u8; 16];
            ip_bytes.copy_from_slice(&buf[8..24]);
            let ip = Ipv6Address::from_octets(ip_bytes);
            Ok(IpEndpoint::new(IpAddress::Ipv6(ip), port))
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn write_sockaddr(
    proc: &Arc<Proc>,
    endpoint: IpEndpoint,
    addr_ptr: usize,
    addrlen_ptr: usize,
) -> Result<()> {
    if addr_ptr == 0 || addrlen_ptr == 0 {
        return Ok(());
    }
    let vmar = proc.vmspace();

    let mut user_len_bytes = [0u8; 4];
    let mut reader = vmar
        .vm_space()
        .reader(addrlen_ptr, 4)
        .map_err(|_| Errno::EFAULT)?;
    let mut writer = ostd::mm::VmWriter::from(&mut user_len_bytes[..]);
    reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;
    let user_len = u32::from_ne_bytes(user_len_bytes) as usize;

    match endpoint.addr {
        IpAddress::Ipv4(v4) => {
            let mut sin = [0u8; 16];
            sin[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
            sin[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
            sin[4..8].copy_from_slice(&v4.octets());

            let copy_len = user_len.min(16);
            if copy_len > 0 {
                let mut writer = vmar
                    .vm_space()
                    .writer(addr_ptr, copy_len)
                    .map_err(|_| Errno::EFAULT)?;
                let mut reader = ostd::mm::VmReader::from(&sin[..copy_len]);
                writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
            }

            let ret_len = 16u32.to_ne_bytes();
            let mut writer = vmar
                .vm_space()
                .writer(addrlen_ptr, 4)
                .map_err(|_| Errno::EFAULT)?;
            let mut reader = ostd::mm::VmReader::from(&ret_len[..]);
            writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
        }
        IpAddress::Ipv6(v6) => {
            let mut sin6 = [0u8; 28];
            sin6[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
            sin6[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
            sin6[8..24].copy_from_slice(&v6.octets());

            let copy_len = user_len.min(28);
            if copy_len > 0 {
                let mut writer = vmar
                    .vm_space()
                    .writer(addr_ptr, copy_len)
                    .map_err(|_| Errno::EFAULT)?;
                let mut reader = ostd::mm::VmReader::from(&sin6[..copy_len]);
                writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
            }

            let ret_len = 28u32.to_ne_bytes();
            let mut writer = vmar
                .vm_space()
                .writer(addrlen_ptr, 4)
                .map_err(|_| Errno::EFAULT)?;
            let mut reader = ostd::mm::VmReader::from(&ret_len[..]);
            writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
        }
    }

    Ok(())
}

pub fn sys_socket(
    domain: i32,
    sock_type: i32,
    protocol: i32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;

    let nonblocking = (sock_type & 0o4000) != 0;
    let cloexec = (sock_type & 0o2000000) != 0;
    let base_type = sock_type & !(0o4000 | 0o2000000);

    if domain != AF_INET && domain != AF_INET6 {
        crate::return_errno!(EAFNOSUPPORT, "unsupported socket domain");
    }

    let socket = match base_type {
        SOCK_STREAM => {
            if protocol != 0 && protocol != IPPROTO_TCP {
                crate::return_errno!(EPROTONOSUPPORT, "unsupported protocol for stream socket");
            }
            KernelSocket::new_tcp(domain)?
        }
        SOCK_DGRAM => {
            if protocol != 0 && protocol != IPPROTO_UDP {
                crate::return_errno!(EPROTONOSUPPORT, "unsupported protocol for dgram socket");
            }
            KernelSocket::new_udp(domain)?
        }
        _ => crate::return_errno!(ESOCKTNOSUPPORT, "unsupported socket type"),
    };

    if nonblocking {
        *socket.nonblocking.lock() = true;
    }

    let file = create_socket_file(socket);
    let mut fd_flags = FdFlags::empty();
    if cloexec {
        fd_flags |= FdFlags::FD_CLOEXEC;
    }

    let fd = proc.fd_table.lock().alloc_fd(file, fd_flags)?;
    Ok(fd as usize)
}

pub fn sys_bind(
    fd: i32,
    addr_ptr: usize,
    addrlen: u32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let endpoint = read_sockaddr(&proc, addr_ptr, addrlen)?;
    socket.bind(endpoint)?;
    Ok(0)
}

pub fn sys_connect(
    fd: i32,
    addr_ptr: usize,
    addrlen: u32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let endpoint = read_sockaddr(&proc, addr_ptr, addrlen)?;
    socket.connect(endpoint)?;
    Ok(0)
}

pub fn sys_listen(
    fd: i32,
    backlog: i32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    socket.listen(backlog)?;
    Ok(0)
}

pub fn sys_accept(
    fd: i32,
    addr_ptr: usize,
    addrlen_ptr: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;

    let (client_sock, remote_endpoint) = socket.accept()?;

    if addr_ptr != 0 && addrlen_ptr != 0 {
        let _ = write_sockaddr(&proc, remote_endpoint, addr_ptr, addrlen_ptr);
    }

    let file = create_socket_file(client_sock);
    let new_fd = proc.fd_table.lock().alloc_fd(file, FdFlags::empty())?;
    Ok(new_fd as usize)
}

pub fn sys_sendto(
    fd: i32,
    buf_ptr: usize,
    len: usize,
    _flags: i32,
    dest_ptr: usize,
    addrlen: u32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    if len == 0 {
        return Ok(0);
    }
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;

    let mut kbuf = alloc::vec![0u8; len.min(65536)];
    let copy_len = kbuf.len();

    let vmar = proc.vmspace();
    let mut reader = vmar
        .vm_space()
        .reader(buf_ptr, copy_len)
        .map_err(|_| Errno::EFAULT)?;
    let mut writer = ostd::mm::VmWriter::from(&mut kbuf[..]);
    reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;

    let endpoint = if dest_ptr != 0 && addrlen > 0 {
        Some(read_sockaddr(&proc, dest_ptr, addrlen)?)
    } else {
        None
    };

    socket.sendto(&kbuf[..copy_len], endpoint)
}

pub fn sys_recvfrom(
    fd: i32,
    buf_ptr: usize,
    len: usize,
    _flags: i32,
    src_ptr: usize,
    addrlen_ptr: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    if len == 0 {
        return Ok(0);
    }
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;

    let mut kbuf = alloc::vec![0u8; len.min(65536)];
    let (nrecv, endpoint_opt) = socket.recvfrom(&mut kbuf)?;

    if nrecv > 0 {
        let vmar = proc.vmspace();
        let mut writer = vmar
            .vm_space()
            .writer(buf_ptr, nrecv)
            .map_err(|_| Errno::EFAULT)?;
        let mut reader = ostd::mm::VmReader::from(&kbuf[..nrecv]);
        writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
    }

    if src_ptr != 0 && addrlen_ptr != 0 {
        if let Some(endpoint) = endpoint_opt {
            let _ = write_sockaddr(&proc, endpoint, src_ptr, addrlen_ptr);
        }
    }

    Ok(nrecv)
}

pub fn sys_shutdown(
    fd: i32,
    how: i32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    socket.shutdown(how)?;
    Ok(0)
}

pub fn sys_getsockname(
    fd: i32,
    addr_ptr: usize,
    addrlen_ptr: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let endpoint = socket.getsockname()?;
    write_sockaddr(&proc, endpoint, addr_ptr, addrlen_ptr)?;
    Ok(0)
}

pub fn sys_getpeername(
    fd: i32,
    addr_ptr: usize,
    addrlen_ptr: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let endpoint = socket.getpeername()?;
    write_sockaddr(&proc, endpoint, addr_ptr, addrlen_ptr)?;
    Ok(0)
}

pub fn sys_setsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval_ptr: usize,
    optlen: u32,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;

    let mut val = [0u8; 4];
    if optlen > 0 && optval_ptr != 0 {
        let copy_len = (optlen as usize).min(4);
        let vmar = proc.vmspace();
        let mut reader = vmar
            .vm_space()
            .reader(optval_ptr, copy_len)
            .map_err(|_| Errno::EFAULT)?;
        let mut writer = ostd::mm::VmWriter::from(&mut val[..copy_len]);
        reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;
    }

    let _ = (socket, level, optname);
    Ok(0)
}

pub fn sys_getsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval_ptr: usize,
    optlen_ptr: usize,
    _ctx: &mut UserContext,
) -> Result<usize> {
    let socket = get_socket(fd)?;
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;

    if optval_ptr == 0 || optlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let val: i32 = match (level, optname) {
        (1, 2) => 1,                // SO_REUSEADDR
        (1, 3) => socket.sock_type, // SO_TYPE
        (1, 4) => 0,                // SO_ERROR
        (6, 1) => 1,                // TCP_NODELAY
        _ => 0,
    };

    let val_bytes = val.to_ne_bytes();
    let vmar = proc.vmspace();
    let mut writer = vmar
        .vm_space()
        .writer(optval_ptr, 4)
        .map_err(|_| Errno::EFAULT)?;
    let mut reader = ostd::mm::VmReader::from(&val_bytes[..]);
    writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;

    let len_bytes = 4u32.to_ne_bytes();
    let mut writer_len = vmar
        .vm_space()
        .writer(optlen_ptr, 4)
        .map_err(|_| Errno::EFAULT)?;
    let mut reader_len = ostd::mm::VmReader::from(&len_bytes[..]);
    writer_len.write_fallible(&mut reader_len).map_err(|_| Errno::EFAULT)?;

    Ok(0)
}
