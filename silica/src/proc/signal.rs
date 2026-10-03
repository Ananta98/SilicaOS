// SPDX-License-Identifier: GPL-2.0

//! Process signal operations (FreeBSD `kern_sig.c`, `kern_sigqueue.c`).
//!
//! The shapes live in [`crate::api::signal`] and the frame layout in
//! [`crate::arch::x86_64::signal`]. This module is the part that acts: holding a
//! signal until a thread can take it, deciding whether a disposition applies, and
//! running the default action.
//!
//! # Delivery, end to end
//!
//! 1. A sender calls `kill(2)`, or the kernel raises one for a fault.
//!    [`Proc::send_signal`] queues it against the *process*: a signal to a
//!    process has to work even when no thread can take it yet.
//! 2. [`Thread::handle_ast`] runs at every return into the kernel, takes the
//!    lowest-numbered pending signal its mask allows, and either enters the
//!    handler or runs the default action.
//!
//! # What a signal cannot do yet
//!
//! * A signal does not interrupt a running thread. It is taken at the next
//!   return into the kernel, which for a tight loop with no system calls is the
//!   next timer tick.
//! * `SIGKILL` cannot reach a thread that is blocked rather than running. OSTD
//!   has no way to cancel a task, so a thread stopped in `wait(2)` waits for
//!   whatever wakes it rather than being killed.
//! * `Proc::wake_one_thread` only sets a flag. Nothing in `sched` reads it, so it
//!   does not make a stopped thread runnable. Pairing it with a wake needs a
//!   `Waker` the blocked thread published (`ostd::sync::Waiter::new_pair`) or a
//!   `Task::run`, which is the same missing piece [`wait`] depends on.

use alloc::vec::Vec;

use ostd::sync::SpinLock;

use crate::{
    api::{
        errno::{Errno, Result},
        signal::{SigHandler, SigInfo, SigSet, Signal},
    },
    proc::{Proc, ProcState, thread::Thread},
};

/// A signal waiting to be delivered.
#[derive(Clone, Copy, Debug)]
pub struct QueuedSignal {
    /// Carries the number, the sender's code, and whatever the signal means by
    /// an address or a status.
    pub info: SigInfo,
}

impl QueuedSignal {
    pub fn signal(&self) -> Signal {
        Signal::from_u32(self.info.si_signo as u32).expect("siginfo_t was built with a known signal")
    }
}

/// Signals waiting for a process.
///
/// A `Vec` because the payloads differ and the ordering rule is per-signal: a
/// second standard signal of the same number is collapsed onto the first, while
/// a different number queues behind it.
pub struct SigQueue {
    inner: SpinLock<Vec<QueuedSignal>>,
}

impl Default for SigQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SigQueue {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(Vec::new()),
        }
    }

    /// Queues a signal, unless one of the same number is already waiting.
    ///
    /// POSIX does not queue the standard signals, so a repeat collapses. The
    /// exception is `SIGCHLD`, where losing one would lose the fact that a child
    /// changed state.
    pub fn post(&self, queued: QueuedSignal) {
        let sig = queued.signal();
        let mut queue = self.inner.lock();
        let collapse = sig != Signal::SIGCHLD && queue.iter().any(|q| q.signal() == sig);
        if !collapse {
            queue.push(queued);
        }
    }

    /// Removes the lowest-numbered queued signal that `mask` does not block.
    pub fn take_unmasked(&self, mask: &SigSet) -> Option<QueuedSignal> {
        let mut queue = self.inner.lock();
        let idx = (0..queue.len())
            .filter(|&i| !mask.contains(queue[i].signal()))
            .min_by_key(|&i| queue[i].signal().as_u32())?;
        Some(queue.remove(idx))
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

impl Proc {
    /// Sends a signal to this process.
    pub fn send_signal(&self, info: SigInfo, sender_pid: u32) -> Result<()> {
        let sig = Signal::from_u32(info.si_signo as u32).ok_or(Errno::EINVAL)?;

        // A zombie can still be sent SIGKILL and SIGCHLD -- a parent needs to
        // learn that its child changed state -- and nothing else, because there
        // is no thread left to act on it.
        if self.state() == ProcState::Zombie && !(sig.is_unblockable() || sig == Signal::SIGCHLD) {
            return Err(Errno::ESRCH);
        }

        // An ignored signal needs no queue entry, and a process with no handler
        // takes the default action when a thread next looks, not when it is sent.
        if matches!(self.sigacts.lock().get(sig).sa_handler, SigHandler::Ignore) {
            return Ok(());
        }

        // A process with no thread that has a user context is a kernel-only one --
        // a daemon like the reclaimer, or `kthread_add`. There is no address space
        // to build a frame in and no user code for a handler to return to, so it is
        // not a signal target at all.
        let has_user_thread = self
            .inner
            .lock()
            .threads
            .iter()
            .any(|t| t.inner.lock().user_ctx.is_some());
        if !has_user_thread {
            return Err(Errno::ESRCH);
        }

        let _ = sender_pid;
        self.sigqueue.post(QueuedSignal { info });
        self.wake_one_thread();
        Ok(())
    }

    /// Arms the AST flag on one thread, preferring one that does not block a
    /// deliverable signal.
    ///
    /// TODO: this sets a flag and nothing observes it outside this module, so a
    /// thread that is stopped rather than running never takes delivery. See the
    /// module documentation.
    fn wake_one_thread(&self) {
        let inner = self.inner.lock();
        for thread in inner.threads.iter() {
            let mut td_inner = thread.inner.lock();
            if !td_inner.flags.contains(crate::proc::thread::ThreadFlags::KILL_PENDING) {
                td_inner.flags.insert(crate::proc::thread::ThreadFlags::TDF_ASTPENDING);
                return;
            }
        }
    }
}

/// Runs the default action for a signal with no handler installed.
///
/// Returns `true` when the calling thread has been terminated and must not
/// return to user space.
pub fn run_default_action(td: &Thread, sig: Signal) -> bool {
    use crate::proc::job_control;

    if sig.default_is_ignore() {
        return false;
    }
    if sig.default_is_stop() {
        job_control::stop_thread(td, sig);
        return false;
    }

    super::exit::exit1(
        td,
        crate::proc::ExitStatus::Signaled {
            signal: sig,
            core_dumped: sig.default_dumps_core(),
        },
    );
    true
}