// SPDX-License-Identifier: GPL-2.0

//! Process waiting and status notification (FreeBSD `kern_wait.c`).
//!
//! Implements `kern_wait6` for observing child process state changes (termination,
//! stopping, continuation) and reaping zombies.

use alloc::{sync::Arc, vec::Vec};
use bitflags::bitflags;
use ostd::sync::WaitQueue;

use crate::errno::Result;
use super::{
    ExitStatus, Proc, ProcState,
    exit::proc_reap,
    tree::{Pid, allproc_find},
};

bitflags! {
    /// Options for `wait6(2)`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct WaitOptions: u32 {
        /// Do not block if no child has exited.
        const WNOHANG    = 1 << 0;
        /// Wait for processes that have exited.
        const WEXITED    = 1 << 1;
        /// Wait for processes that have stopped.
        const WSTOPPED   = 1 << 2;
        /// Wait for processes that have continued.
        const WCONTINUED = 1 << 3;
        /// Keep the process in a waitable state (do not reap).
        const WNOWAIT    = 1 << 4;
    }
}

/// Identification target type for `wait6(2)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdType {
    /// Any child process.
    All,
    /// Specific child PID.
    Pid(Pid),
    /// Process group.
    Pgid(u32),
}

/// Wait result payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitResult {
    pub pid: Pid,
    pub status: ExitStatus,
    pub exit_code: i32,
    pub uid: u32,
}

/// Global wait queue for process waiting.
static WAIT_QUEUE: WaitQueue = WaitQueue::new();

/// Wakes up any tasks waiting on child events.
pub fn notify_waiters(_parent_pid: Pid) {
    WAIT_QUEUE.wake_all();
}

/// FreeBSD `kern_wait6`: waits for child status changes and optionally reaps zombies.
pub fn kern_wait6(
    parent: &Arc<Proc>,
    idtype: IdType,
    options: WaitOptions,
) -> Result<Option<WaitResult>> {
    loop {
        // Collect children to inspect
        let children = parent.inner.lock().children.clone();
        if children.is_empty() {
            crate::return_errno!(ECHILD, "no child processes exist");
        }

        // Filter by idtype
        let candidates: Vec<Pid> = children
            .into_iter()
            .filter(|&child_pid| match idtype {
                IdType::All => true,
                IdType::Pid(target) => child_pid == target,
                IdType::Pgid(_) => true,
            })
            .collect();

        if candidates.is_empty() {
            crate::return_errno!(ECHILD, "no matching child process found");
        }

        // Check for eligible children
        for &child_pid in &candidates {
            if let Some(child) = allproc_find(child_pid) {
                let (state, status) = {
                    let inner = child.inner.lock();
                    (inner.state, inner.xstat)
                };

                if state == ProcState::Zombie && (options.is_empty() || options.contains(WaitOptions::WEXITED)) {
                    let uid = child.cred().cr_uid.as_u32();
                    let final_status = if options.contains(WaitOptions::WNOWAIT) {
                        status.unwrap_or(ExitStatus::Exited(0))
                    } else {
                        let (_, s) = proc_reap(parent, child_pid)?;
                        s
                    };

                    let exit_code = match final_status {
                        ExitStatus::Exited(c) => c,
                        ExitStatus::Signaled { signal, .. } => 128 + signal.as_u32() as i32,
                        ExitStatus::Stopped(signal) => signal.as_u32() as i32,
                        ExitStatus::Continued => 0,
                    };

                    return Ok(Some(WaitResult {
                        pid: child_pid,
                        status: final_status,
                        exit_code,
                        uid,
                    }));
                }
            }
        }

        // If non-blocking was requested, return Ok(None) immediately
        if options.contains(WaitOptions::WNOHANG) {
            return Ok(None);
        }

        // Wait on wait queue until notified of child state change
        WAIT_QUEUE.wait_until(|| {
            let has_zombie = candidates.iter().any(|&child_pid| {
                allproc_find(child_pid).is_some_and(|c| c.state() == ProcState::Zombie)
            });
            has_zombie.then_some(())
        });
    }
}
