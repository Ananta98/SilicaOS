// SPDX-License-Identifier: GPL-2.0

//! Job control (POSIX `setsid`, `setpgid`, and the stop/continue signals).
//!
//! A process group is the unit `wait(2)`'s `IdType::Pgid` matches against and the
//! unit a terminal sends `SIGINT` to. This module owns the group identifier, the
//! stop and continue transitions, and the `ExitStatus` values that let a parent
//! observe them.
//!
//! # Stopping
//!
//! A stopped thread keeps its state and its signal mask and simply stops being
//! scheduled; it is not terminated, and a [`ProcState::Zombie`] is a different
//! thing entirely. The distinction matters because a stopped process can be
//! continued and has to resume with the state it had, whereas a zombie is
//! waiting to be collected and never runs again.
//!
//! # What does not work yet
//!
//! Nothing resumes a stopped thread on its own, and nothing makes one runnable
//! again. [`stop_thread`] marks it and records the state change for the parent;
//! the other half of the transition is a wake, and `sched` offers no way to make
//! an existing task runnable again. A `SIGCONT` therefore resumes nothing, and
//! the TODO on [`continue_thread`] says so.

use alloc::{sync::Arc, vec::Vec};

use crate::{
    api::{
        errno::{Errno, Result},
        signal::{SigInfo, Signal, code},
    },
    proc::{
        ExitStatus, Proc,
        thread::{Thread, ThreadState},
    },
};

impl Proc {
    /// The process group this process belongs to.
    ///
    /// Falls back to the process's own identifier, which is what a process that
    /// has not called `setpgid` or `setsid` belongs to.
    pub fn pgid(&self) -> crate::proc::tree::Pid {
        self.inner
            .lock()
            .pgid
            .unwrap_or(self.pid)
    }

    /// Moves this process into the group `pgid`.
    ///
    /// POSIX only allows a process to change its own group, or that of a child it
    /// has not yet waited for. Anything else returns `EPERM` rather than silently
    /// doing something the caller did not ask for.
    pub fn set_pgid(&self, pgid: crate::proc::tree::Pid) -> Result<()> {
        let caller = Thread::current_proc().ok_or(Errno::ESRCH)?;

        if caller.pid != self.pid && !caller.is_parent_of(self.pid) {
            return Err(Errno::EPERM);
        }
        // The group has to exist, or at least be this process: a group is created
        // by its leader, so the only group a process can be moved into that
        // already exists is one led by a live process.
        let leader_exists =
            crate::proc::tree::allproc_find(pgid).is_some_and(|p| p.pid == pgid);
        if !leader_exists {
            return Err(Errno::EPERM);
        }

        self.inner.lock().pgid = Some(pgid);
        Ok(())
    }

    /// Puts this process in a new group with itself as the leader.
    ///
    /// Returns the new group identifier, or `EPERM` if the caller is already a
    /// group leader, which is what makes `setsid` fail for a session leader.
    pub fn setsid(&self) -> Result<crate::proc::tree::Pid> {
        if self.pgid() == self.pid {
            return Err(Errno::EPERM);
        }
        let sid = self.pid;
        self.inner.lock().pgid = Some(sid);
        Ok(sid)
    }

    fn is_parent_of(&self, child: crate::proc::tree::Pid) -> bool {
        self.inner.lock().children.contains(&child)
    }
}

/// Stops every thread of the process that `td` belongs to.
///
/// The signal stops the *process*, not the thread that happened to take delivery
/// of it, so every thread is marked. Only the thread that takes delivery is the
/// one the kernel called, and stopping just that one would leave the process
/// running on the others.
pub fn stop_thread(td: &Thread, sig: Signal) {
    let Some(proc) = td.proc() else {
        return;
    };

    let threads: Vec<Arc<Thread>> = proc.inner.lock().threads.clone();
    for thread in threads.iter() {
        let mut inner = thread.inner.lock();
        // A thread already on its way out is left alone: a zombie is not stopped.
        if inner.state != ThreadState::Dead {
            inner.state = ThreadState::Stopped;
            inner.stopped_by = Some(sig);
        }
    }

    // Record the transition for whoever is waiting. The parent has to be told even
    // if it has asked not to be, because `wait4` with `WUNTRACED` is how a shell
    // learns a child stopped; the disposition is only consulted for the `SIGCHLD`
    // the shell sees.
    if let Some(parent) = proc.inner.lock().ppid.and_then(crate::proc::tree::allproc_find) {
        let _ = parent.send_signal(
            SigInfo {
                si_signo: Signal::SIGCHLD.as_u32() as i32,
                si_code: code::USER,
                si_pid: proc.pid.as_u32() as i32,
                si_status: StoppedStatus::Stopped.into_raw(),
                ..SigInfo::default()
            },
            proc.pid.as_u32(),
        );
    }
}

/// Resumes a stopped process.
///
/// TODO: this clears the state, but nothing makes the threads runnable again.
/// `sched` has no way to re-enqueue an existing `Task`, so a resumed process
/// stays where it was until something else happens to wake it -- which, for a
/// process stopped in `wait(2)`, means it never resumes at all.
pub fn continue_thread(proc: &Arc<Proc>) {
    let threads: Vec<Arc<Thread>> = proc.inner.lock().threads.clone();
    for thread in threads.iter() {
        let mut inner = thread.inner.lock();
        if inner.state == ThreadState::Stopped {
            inner.state = ThreadState::CanRun;
            inner.stopped_by = None;
        }
    }
}

/// The status a parent sees for a child that stopped or continued.
///
/// Encoded into `siginfo_t`'s `si_status` for `SIGCHLD`, and decoded from the
/// `wstatus` word by `wait4`.
pub enum StoppedStatus {
    /// The child stopped because of a signal.
    Stopped,
    /// The child was resumed.
    Continued,
}

impl StoppedStatus {
    /// The value `si_status` carries.
    pub const fn into_raw(self) -> i32 {
        match self {
            Self::Stopped => 0x7f,
            Self::Continued => 0xffff,
        }
    }
}

/// Turns a stop into the [`ExitStatus`] a parent waiting for one should see.
pub fn stopped_status(sig: Signal) -> ExitStatus {
    ExitStatus::Stopped(sig)
}