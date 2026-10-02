// SPDX-License-Identifier: GPL-2.0

//! Thread management (FreeBSD `kern_thread.c`).
//!
//! A [`Thread`] represents an execution context within a [`Proc`]. It wraps an
//! underlying [`ostd::task::Task`], maintains user-space CPU contexts, signal masks,
//! and handles transitions between Ring 0 and Ring 3 via [`ostd::user::UserMode`].

use alloc::sync::{Arc, Weak};
use core::{
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};
use bitflags::bitflags;
use ostd::{
    arch::cpu::context::{CpuException, UserContext},
    sync::SpinLock,
    task::{Task, TaskOptions},
    user::{DummyUserHooks, ReturnReason, UserMode},
};

use super::{
    COMM_LEN,
    ExitStatus,
    Proc,
    signal::{SigQueue, SigSet, Signal},
};

/// Thread identifier wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid(pub NonZeroU32);

impl Tid {
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

impl core::fmt::Display for Tid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_u32())
    }
}

static NEXT_TID: AtomicU32 = AtomicU32::new(1);

/// The largest thread identifier handed out before `alloc_tid` wraps.
pub const TID_MAX: u32 = u32::MAX / 2;

/// Allocates a unique TID.
///
/// Wrapping restarts at 1 rather than 0. `Tid` wraps a [`NonZeroU32`], and a plain
/// `fetch_add` that reached 0 would be forced back onto 1 -- the identifier init's
/// thread already owns -- and so would hand out a live TID.
///
/// TODO: identifiers are never reclaimed, so the space eventually wraps and can
/// collide with a thread that is still alive. Reuse needs a free list fed by
/// [`super::Proc::remove_thread`], which nothing calls yet.
pub fn alloc_tid() -> Tid {
    let id = NEXT_TID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(if cur >= TID_MAX { 1 } else { cur + 1 })
        })
        // `fetch_update` only fails when the closure returns `None`, which it never
        // does, so the current value is returned unchanged.
        .unwrap_or(TID_MAX);
    Tid(NonZeroU32::new(id).expect("alloc_tid never yields 0"))
}

/// Builds the OSTD task that backs a thread.
///
/// # Panics
///
/// Not recoverable, and deliberately so. A thread without a task to run has no
/// meaning, and the callers are inside `Arc::new_cyclic`, whose closure returns
/// the `Thread` and so cannot hand a failure back. Every caller already returns
/// `Result` for its other fallible step (allocating a PID), which makes an
/// `expect` here read as though the error could be propagated.
pub fn build_task(options: TaskOptions, nice: i8) -> Arc<Task> {
    match crate::sched::build(options, nice) {
        Ok(task) => task,
        Err(err) => panic!("cannot create a task for a new thread: {err:?}"),
    }
}

bitflags! {
    /// Thread flags mirroring FreeBSD `TDF_*`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct ThreadFlags: u32 {
        /// Asynchronous System Trap pending (signals, resched).
        ///
        /// Nothing outside `proc` reads this yet, so setting it does not by itself
        /// get the thread to run again. See [`super::Proc::post_signal`].
        const TDF_ASTPENDING   = 1 << 0;
        /// Thread is a kernel-only thread.
        const TDF_KTHREAD      = 1 << 2;
    }
}

/// Thread lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    CanRun,
    Running,
    Sleeping,
    /// Stopped by a job-control signal.
    ///
    /// TODO: job control. Nothing sets this back to `CanRun` -- SIGSTOP and SIGTSTP
    /// have no continuation path, and `SIGCONT` is not delivered -- and
    /// [`Thread::user_loop`] never tests for it, so a thread marked `Stopped` keeps
    /// running user code.
    Stopped,
    Dead,
}

/// Mutable thread state protected by `Thread::inner`.
pub struct ThreadInner {
    pub state: ThreadState,
    pub flags: ThreadFlags,
    pub sigmask: SigSet,
    pub sigqueue: SigQueue,
    /// The object this thread is blocked on, for diagnostics.
    ///
    /// TODO: blocking is done by parking on an `ostd::sync::WaitQueue`, which keeps
    /// the reason inside the closure rather than here, so nothing ever sets this.
    pub wchan: Option<usize>,
    /// A printable name for `wchan`.
    ///
    /// TODO: see [`Self::wchan`]; never set.
    pub wmesg: &'static str,
    pub name: [u8; COMM_LEN],
    pub user_ctx: Option<UserContext>,
}

/// Thread Control Block (FreeBSD `struct thread`).
pub struct Thread {
    /// Unique thread identifier.
    pub tid: Tid,
    /// Weak back-reference to the containing Process. Prevents reference cycles.
    ///
    /// Weak means a thread outlives its process if it is still running: the last
    /// strong [`Proc`] reference goes away when the zombie is reaped, and
    /// [`Self::proc`] then returns `None`.
    pub td_proc: Weak<Proc>,
    /// Underlying execution context provided by OSTD.
    pub td_kstack: Arc<Task>,
    /// Mutable thread state.
    pub inner: SpinLock<ThreadInner>,
}

impl Thread {
    /// Upgrades the weak process pointer to retrieve the parent process.
    pub fn proc(&self) -> Option<Arc<Proc>> {
        self.td_proc.upgrade()
    }

    /// Resolves the currently running `Thread` on the current CPU via `Task::data()`.
    pub fn current() -> Option<Arc<Thread>> {
        let task = Task::current()?;
        let weak_td = task.data().downcast_ref::<Weak<Thread>>()?;
        weak_td.upgrade()
    }

    /// Resolves the currently running `Proc`.
    pub fn current_proc() -> Option<Arc<Proc>> {
        Self::current()?.proc()
    }

    /// Marks the thread runnable and notifies OSTD scheduler.
    pub fn run(&self) {
        self.td_kstack.run();
    }

    /// Posts a thread-directed signal.
    pub fn post_signal(&self, sig: Signal) {
        let mut inner = self.inner.lock();
        inner.sigqueue.post(sig);
        inner.flags.insert(ThreadFlags::TDF_ASTPENDING);
    }

    /// Checks if unmasked signals are pending and handles them.
    pub fn handle_ast(&self) -> bool {
        {
            let inner = self.inner.lock();
            if !inner.flags.contains(ThreadFlags::TDF_ASTPENDING) {
                return false;
            }
        }

        let sigmask = self.inner.lock().sigmask;
        // Check thread queue then process queue
        let thread_sig = self.inner.lock().sigqueue.pop_unmasked(sigmask);

        let sig = if let Some(sig) = thread_sig {
            Some(sig)
        } else if let Some(proc) = self.proc() {
            let mut proc_queue = proc.sigqueue.lock();
            proc_queue.pop_unmasked(sigmask)
        } else {
            None
        };

        if let Some(sig) = sig {
            self.deliver_signal(sig);
            return true;
        }

        // Nothing was deliverable. Only stand down if both queues are actually
        // empty: a signal that is still masked is not lost, it is deferred until
        // the mask is opened, and clearing the flag here would strand it with
        // nothing left to re-arm the trap.
        let thread_queue_empty = self.inner.lock().sigqueue.is_empty();
        let proc_queue_empty = self
            .proc()
            .is_none_or(|proc| proc.sigqueue.lock().is_empty());

        if thread_queue_empty && proc_queue_empty {
            self.inner.lock().flags.remove(ThreadFlags::TDF_ASTPENDING);
            return false;
        }
        true
    }

    /// Delivers a signal or executes default action.
    fn deliver_signal(&self, sig: Signal) {
        let proc = match self.proc() {
            Some(p) => p,
            None => return,
        };

        let action = proc.sigacts.lock().get(sig);
        match action.sa_handler {
            super::signal::SigHandler::Ignore => {}
            super::signal::SigHandler::Default => {
                match sig {
                    Signal::SIGCHLD | Signal::SIGCONT | Signal::SIGWINCH => {
                        // Default is ignore
                    }
                    Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU => {
                        // Stop thread/process
                        self.inner.lock().state = ThreadState::Stopped;
                    }
                    _ => {
                        // Default is terminate process
                        super::exit::exit1(self, ExitStatus::Signaled {
                            signal: sig,
                            core_dumped: false,
                        });
                    }
                }
            }
            super::signal::SigHandler::Handler(_handler_addr) => {
                // TODO: user handlers. Nothing sets a disposition either --
                // `rt_sigaction` does not exist, so `SigHandler::Handler` is
                // unreachable -- and a handler needs a signal frame built on the
                // user stack plus a restorer trampoline, neither of which exists.
                ostd::info!("Signal {} handled by user handler", sig.as_u32());
            }
        }
    }

    /// Primary userspace execution loop for user threads.
    ///
    /// Returns when the thread dies, or when its process has been collected and
    /// `Self::proc` can no longer be resolved.
    pub fn user_loop(&self, initial_ctx: UserContext) {
        // The process holds the only strong references to its threads, so reaping
        // the last of them drops it. A thread that is still running at that point
        // has no process left and must not keep executing user code.
        let Some(proc) = self.proc() else {
            return;
        };
        let vmar = proc.vmspace();
        vmar.activate();

        let mut user_mode = UserMode::new(initial_ctx);

        loop {
            // Check AST / pending signals prior to entering Ring 3
            self.handle_ast();
            if self.is_dead() {
                return;
            }

            let return_reason = user_mode.execute(&DummyUserHooks);

            match return_reason {
                ReturnReason::UserSyscall => {
                    crate::syscall::dispatch(user_mode.context_mut());
                }
                ReturnReason::UserException => {
                    if let Some(exception) = user_mode.context_mut().take_exception() {
                        match exception {
                            CpuException::PageFault(raw_pf) => {
                                let info = crate::vm::vmar::page_fault::PageFaultInfo::from_fault(
                                    raw_pf.addr,
                                    raw_pf.error_code,
                                );
                                match info.and_then(|info| vmar.handle_page_fault(&info).err()) {
                                    Some(err) => {
                                        ostd::error!(
                                            "Fatal user page fault at {:#x}: {:?}",
                                            raw_pf.addr,
                                            err
                                        );
                                        self.deliver_signal(Signal::SIGSEGV);
                                    }
                                    None if info.is_none() => {
                                        ostd::error!(
                                            "User fault outside user range: {:#x}",
                                            raw_pf.addr
                                        );
                                        self.deliver_signal(Signal::SIGSEGV);
                                    }
                                    None => {}
                                }
                            }
                            // TODO: map every exception to the signal POSIX
                            // requires. This reports SIGILL for all of them, so a
                            // divide-by-zero or a general-protection fault is
                            // indistinguishable from a bad opcode.
                            other => {
                                ostd::error!("Unhandled user CPU exception: {:?}", other);
                                self.deliver_signal(Signal::SIGILL);
                            }
                        }
                    }
                }
                ReturnReason::KernelEvent => {
                    // Preempted or kernel event occurred, loop back to check AST
                }
            }

            // A signal handled above may have run the default terminating action,
            // which is what marks the thread dead.
            if self.is_dead() {
                return;
            }
        }
    }

    /// Returns whether this thread has been terminated.
    fn is_dead(&self) -> bool {
        self.inner.lock().state == ThreadState::Dead
    }
}
