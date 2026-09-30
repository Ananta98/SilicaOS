// SPDX-License-Identifier: GPL-2.0

//! Global process table and PID allocator (FreeBSD `allproc`, `proctree_lock`).
//!
//! Manages process tree hierarchy, process lookups, PID allocation, and reparenting
//! to designated reapers or init (PID 1).

use alloc::{collections::{BTreeMap, BTreeSet}, sync::Arc};
use core::num::NonZeroU32;
use ostd::sync::{RwLock, SpinLock};

use crate::errno::Result;
use super::Proc;

/// Maximum process identifier value.
pub const PID_MAX: u32 = 99999;

/// Fixed PID for the init process.
pub const PID_INIT: Pid = Pid(NonZeroU32::MIN);

/// Unique process identifier wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(pub NonZeroU32);

impl Pid {
    pub const fn from_u32(val: u32) -> Option<Self> {
        match NonZeroU32::new(val) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    pub const fn as_u32(&self) -> u32 {
        self.0.get()
    }
}

impl core::fmt::Display for Pid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_u32())
    }
}

/// Dynamic PID allocator ensuring uniqueness and cyclic reuse.
pub struct PidAllocator {
    next: u32,
    allocated: BTreeSet<u32>,
}

impl PidAllocator {
    pub const fn new() -> Self {
        Self {
            next: 1,
            allocated: BTreeSet::new(),
        }
    }

    /// Allocates the next available PID.
    pub fn allocate(&mut self) -> Result<Pid> {
        let start = self.next;
        loop {
            let candidate = self.next;
            self.next = if self.next >= PID_MAX { 1 } else { self.next + 1 };

            if !self.allocated.contains(&candidate) {
                self.allocated.insert(candidate);
                let pid = Pid::from_u32(candidate).expect("candidate is >= 1");
                return Ok(pid);
            }

            if self.next == start {
                crate::return_errno!(EAGAIN, "no available process IDs");
            }
        }
    }

    /// Frees an allocated PID.
    pub fn free(&mut self, pid: Pid) {
        self.allocated.remove(&pid.as_u32());
    }

    /// Reserves a specific PID (e.g. PID 1 for init).
    pub fn reserve(&mut self, pid: Pid) -> Result<()> {
        let val = pid.as_u32();
        if self.allocated.contains(&val) {
            crate::return_errno!(EEXIST, "PID {val} is already allocated");
        }
        self.allocated.insert(val);
        Ok(())
    }
}

impl Default for PidAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Process tree structure containing all active and zombie processes.
pub struct ProcessTree {
    pub allproc: BTreeMap<Pid, Arc<Proc>>,
}

impl ProcessTree {
    pub const fn new() -> Self {
        Self {
            allproc: BTreeMap::new(),
        }
    }
}

impl Default for ProcessTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Global locks.
pub static PID_ALLOCATOR: SpinLock<PidAllocator> = SpinLock::new(PidAllocator::new());
pub static PROCTREE_LOCK: RwLock<ProcessTree> = RwLock::new(ProcessTree::new());

/// Looks up an active or zombie process by PID.
pub fn allproc_find(pid: Pid) -> Option<Arc<Proc>> {
    let tree = PROCTREE_LOCK.read();
    tree.allproc.get(&pid).cloned()
}

/// Registers a new process into `allproc`.
pub fn allproc_insert(proc: Arc<Proc>) {
    let mut tree = PROCTREE_LOCK.write();
    tree.allproc.insert(proc.pid, proc);
}

/// Removes a process from `allproc` (e.g. after being reaped).
pub fn allproc_remove(pid: Pid) -> Option<Arc<Proc>> {
    let mut tree = PROCTREE_LOCK.write();
    tree.allproc.remove(&pid)
}

/// Returns the number of processes in `allproc`.
pub fn allproc_count() -> usize {
    let tree = PROCTREE_LOCK.read();
    tree.allproc.len()
}

/// Finds the designated reaper for `proc` by traversing up the ancestor chain.
/// If no reaper process is configured, defaults to PID 1 (init).
pub fn tree_find_reaper(proc: &Proc) -> Pid {
    let mut current_ppid = proc.inner.lock().ppid;
    while let Some(ppid) = current_ppid {
        if let Some(parent) = allproc_find(ppid) {
            let inner = parent.inner.lock();
            if inner.is_reaper {
                return ppid;
            }
            current_ppid = inner.ppid;
        } else {
            break;
        }
    }
    PID_INIT
}

/// Reparents a list of children to `reaper_pid`.
pub fn tree_reparent_children(children: &[Pid], reaper_pid: Pid) {
    if let Some(reaper) = allproc_find(reaper_pid) {
        let mut reaper_inner = reaper.inner.lock();
        for &child_pid in children {
            if let Some(child) = allproc_find(child_pid) {
                child.inner.lock().ppid = Some(reaper_pid);
                if !reaper_inner.children.contains(&child_pid) {
                    reaper_inner.children.push(child_pid);
                }
            }
        }
    }
}
