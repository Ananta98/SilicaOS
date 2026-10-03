// SPDX-License-Identifier: GPL-2.0

//! Global process table and PID allocator (FreeBSD `allproc`, `proctree_lock`).
//!
//! Manages process tree hierarchy, process lookups, PID allocation, and reparenting
//! to designated reapers or init (PID 1).

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::num::NonZeroU32;
use ostd::sync::{RwLock, SpinLock};

use super::Proc;
use crate::api::errno::Result;

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
    /// The first PID handed out by [`Self::allocate`].
    ///
    /// PID 1 belongs to init, so allocation starts after it rather than at it.
    /// Booting code creates kernel processes before init exists -- the memory
    /// reclaimer registers `kswapd0` first -- and an allocator starting at 1 gave
    /// that daemon init's identifier. Reserving 1 from the start makes that
    /// unrepresentable instead of relying on init being the first process created.
    const FIRST_ALLOCATABLE: u32 = 2;

    pub const fn new() -> Self {
        Self {
            next: Self::FIRST_ALLOCATABLE,
            allocated: BTreeSet::new(),
        }
    }

    /// Allocates the next available PID.
    pub fn allocate(&mut self) -> Result<Pid> {
        let start = self.next;
        loop {
            let candidate = self.next;
            self.next = if self.next >= PID_MAX {
                1
            } else {
                self.next + 1
            };

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

/// Every process in `allproc`, in ascending order of identifier.
///
/// Holds the tree lock for as long as the returned iterator lives, so a caller
/// must not block while holding it.
pub fn allproc_iter() -> impl Iterator<Item = Arc<Proc>> {
    let tree = PROCTREE_LOCK.read();
    let procs: alloc::vec::Vec<Arc<Proc>> = tree.allproc.values().cloned().collect();
    procs.into_iter()
}

/// Returns the number of processes in `allproc`.
pub fn allproc_count() -> usize {
    let tree = PROCTREE_LOCK.read();
    tree.allproc.len()
}

/// Finds the designated reaper for a process by walking up the ancestor chain
/// from `ppid`. If no ancestor is a reaper, defaults to PID 1 (init).
///
/// This takes a [`Pid`] rather than a [`Proc`] on purpose. It has to inspect each
/// ancestor's `ProcInner`, and accepting the whole `Proc` made it natural to call
/// it while that process's own `inner` was already locked — which
/// [`super::exit::exit1`] did, deadlocking on the non-reentrant `SpinLock`.
/// Starting from a bare `ppid` means no lock is ever held on entry.
///
/// # Locking
///
/// Each iteration acquires an ancestor's `inner` and releases it before the next
/// lookup, so no two `Proc::inner` locks are ever held at once.
pub fn tree_find_reaper(ppid: Option<Pid>) -> Pid {
    let mut current = ppid;
    while let Some(candidate) = current {
        let Some(parent) = allproc_find(candidate) else {
            // The ancestor is already gone; fall back to init.
            break;
        };
        let (is_reaper, parent_ppid) = {
            let inner = parent.inner.lock();
            (inner.is_reaper, inner.ppid)
        };
        if is_reaper {
            return candidate;
        }
        // Guard against a cycle introduced by a corrupt reparenting, which would
        // otherwise spin here forever.
        if parent_ppid == current {
            break;
        }
        current = parent_ppid;
    }
    PID_INIT
}

/// Reparents a list of children to `reaper_pid`.
///
/// # Locking
///
/// Each child's `inner` is locked and released in its own iteration, and the
/// reaper's `inner` is taken once at the end. Holding the reaper's lock across
/// the per-child updates would nest two `Proc::inner` locks, and two processes
/// exiting at the same time could then take them in opposite orders and hang.
pub fn tree_reparent_children(children: &[Pid], reaper_pid: Pid) {
    let Some(reaper) = allproc_find(reaper_pid) else {
        return;
    };

    let mut reparented = Vec::new();
    for &child_pid in children {
        if let Some(child) = allproc_find(child_pid) {
            child.inner.lock().ppid = Some(reaper_pid);
            reparented.push(child_pid);
        }
    }

    if reparented.is_empty() {
        return;
    }

    let mut reaper_inner = reaper.inner.lock();
    for child_pid in reparented {
        if !reaper_inner.children.contains(&child_pid) {
            reaper_inner.children.push(child_pid);
        }
    }
}
