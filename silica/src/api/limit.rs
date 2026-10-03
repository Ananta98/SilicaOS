// SPDX-License-Identifier: GPL-2.0

//! Resource limits (FreeBSD `sys/resourcevar.h`).
//!
//! Manages per-process resource limits (`struct plimit` and `struct rlimit`).
//! Resource limits are shared across threads within a process and are cloned
//! across `fork(2)`. Modifications follow Copy-on-Write semantics.

use crate::api::errno::Result;

/// Unbounded resource limit value (POSIX `RLIM_INFINITY`).
pub const RLIM_INFINITY: u64 = !0;

/// Resource limit identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Resource {
    /// CPU time per process in seconds.
    Cpu = 0,
    /// Maximum file size in bytes.
    Fsize = 1,
    /// Maximum data segment size in bytes.
    Data = 2,
    /// Maximum stack size in bytes.
    Stack = 3,
    /// Maximum core file size in bytes.
    Core = 4,
    /// Maximum resident set size in bytes.
    Rss = 5,
    /// Maximum locked memory in bytes.
    Memlock = 6,
    /// Maximum number of processes per UID.
    Nproc = 7,
    /// Maximum number of open file descriptors.
    Nofile = 8,
    /// Maximum address space size in bytes.
    As = 9,
}

impl Resource {
    pub const COUNT: usize = 10;

    pub const fn from_usize(val: usize) -> Option<Self> {
        match val {
            0 => Some(Self::Cpu),
            1 => Some(Self::Fsize),
            2 => Some(Self::Data),
            3 => Some(Self::Stack),
            4 => Some(Self::Core),
            5 => Some(Self::Rss),
            6 => Some(Self::Memlock),
            7 => Some(Self::Nproc),
            8 => Some(Self::Nofile),
            9 => Some(Self::As),
            _ => None,
        }
    }
}

/// Resource limit pair (current soft limit and maximum hard limit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RLimit {
    /// Soft limit (current effective limit).
    pub rlim_cur: u64,
    /// Hard limit (ceiling that non-root processes cannot raise).
    pub rlim_max: u64,
}

impl RLimit {
    pub const fn infinity() -> Self {
        Self {
            rlim_cur: RLIM_INFINITY,
            rlim_max: RLIM_INFINITY,
        }
    }

    pub const fn new(cur: u64, max: u64) -> Self {
        Self {
            rlim_cur: cur,
            rlim_max: max,
        }
    }
}

/// Process resource limits container mirroring FreeBSD `struct plimit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plimit {
    pub limits: [RLimit; Resource::COUNT],
}

impl Plimit {
    /// Creates a default set of resource limits.
    pub fn default_limits() -> Self {
        let mut limits = [RLimit::infinity(); Resource::COUNT];
        // Stack limit default: 8 MiB soft, 64 MiB hard
        limits[Resource::Stack as usize] = RLimit::new(8 * 1024 * 1024, 64 * 1024 * 1024);
        // File descriptors default: 1024 soft, 4096 hard
        limits[Resource::Nofile as usize] = RLimit::new(1024, 4096);
        // Max processes default: 512 soft, 1024 hard
        limits[Resource::Nproc as usize] = RLimit::new(512, 1024);
        Self { limits }
    }

    /// Gets the current limit for a resource.
    pub fn get(&self, res: Resource) -> RLimit {
        self.limits[res as usize]
    }

    /// Sets the limit for a resource with permission validation.
    pub fn set(&mut self, res: Resource, new_lim: RLimit, is_root: bool) -> Result<()> {
        if new_lim.rlim_cur > new_lim.rlim_max {
            crate::return_errno!(EINVAL, "soft limit exceeds hard limit");
        }
        let old_lim = self.limits[res as usize];
        if new_lim.rlim_max > old_lim.rlim_max && !is_root {
            crate::return_errno!(EPERM, "cannot raise hard limit without superuser privilege");
        }
        self.limits[res as usize] = new_lim;
        Ok(())
    }
}

impl Default for Plimit {
    fn default() -> Self {
        Self::default_limits()
    }
}
