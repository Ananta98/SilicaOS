// SPDX-License-Identifier: GPL-2.0

//! I/O multiplexing: `poll(2)` readiness reporting.
//!
//! A file reports its current readiness through [`FileOps::poll`]. Anything
//! that changes readiness (a pipe gaining data, a peer closing, ...) calls
//! [`notify`], which wakes every sleeping poller so it can re-scan its set.
//! That is a deliberate trade: one global queue means a wakeup can be spurious
//! for some pollers, but it needs no per-file registration and cannot lose a
//! wakeup.

use alloc::sync::Arc;
use bitflags::bitflags;
use ostd::sync::WaitQueue;

use crate::api::errno::Result;
use super::vfs::File;

bitflags! {
    /// Event bits, with the Linux x86-64 values so they can cross the ABI.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct PollEvents: u16 {
        /// Data may be read without blocking.
        const IN     = 0x0001;
        /// Urgent data may be read.
        const PRI    = 0x0002;
        /// Data may be written without blocking.
        const OUT    = 0x0004;
        /// An error occurred (output only).
        const ERR    = 0x0008;
        /// The peer hung up (output only).
        const HUP    = 0x0010;
        /// The descriptor is not open (output only).
        const NVAL   = 0x0020;
        const RDNORM = 0x0040;
        const RDBAND = 0x0080;
        const WRNORM = 0x0100;
        const WRBAND = 0x0200;
    }
}

impl PollEvents {
    /// Bits reported regardless of what the caller asked for.
    pub const ALWAYS: Self = Self::ERR.union(Self::HUP).union(Self::NVAL);
}

/// Mirror of `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

/// How long [`poll`] may sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollTimeout {
    /// Scan once and return (`timeout == 0`).
    NoWait,
    /// Sleep until something is ready (`timeout < 0`).
    Infinite,
}

static POLL_WAITQ: WaitQueue = WaitQueue::new();

/// Wakes all pollers; call after any readiness change.
pub fn notify() {
    POLL_WAITQ.wake_all();
}

/// Scans `fds` once, filling `revents`. Returns the number of ready entries.
fn scan(fds: &mut [PollFd], get_file: &dyn Fn(i32) -> Option<Arc<File>>) -> usize {
    let mut ready = 0;
    for pfd in fds.iter_mut() {
        pfd.revents = 0;
        if pfd.fd < 0 {
            continue;
        }
        let revents = match get_file(pfd.fd) {
            None => PollEvents::NVAL,
            Some(file) => {
                let want = PollEvents::from_bits_truncate(pfd.events as u16) | PollEvents::ALWAYS;
                file.poll() & want
            }
        };
        pfd.revents = revents.bits() as i16;
        if !revents.is_empty() {
            ready += 1;
        }
    }
    ready
}

/// Implements the core of `poll(2)`.
///
/// `get_file` resolves a descriptor in the caller's table. Returns the number
/// of descriptors with non-zero `revents`.
///
/// Only `0` and "forever" timeouts are supported; a bounded sleep needs a timer
/// wheel the kernel does not have yet.
pub fn poll(
    fds: &mut [PollFd],
    get_file: &dyn Fn(i32) -> Option<Arc<File>>,
    timeout: PollTimeout,
) -> Result<usize> {
    let ready = scan(fds, get_file);
    if ready > 0 || timeout == PollTimeout::NoWait {
        return Ok(ready);
    }
    Ok(POLL_WAITQ.wait_until(|| {
        let n = scan(fds, get_file);
        if n > 0 { Some(n) } else { None }
    }))
}
