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
    task::Task,
    user::{DummyUserHooks, ReturnReason, UserMode},
};

use super::{
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

/// Allocates a unique TID.
pub fn alloc_tid() -> Tid {
    let id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    Tid(NonZeroU32::new(id).unwrap_or(NonZeroU32::MIN))
}

bitflags! {
    /// Thread flags mirroring FreeBSD `TDF_*`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct ThreadFlags: u32 {
        /// Asynchronous System Trap pending (signals, resched).
        const TDF_ASTPENDING   = 1 << 0;
        /// Preemption requested by scheduler.
        const TDF_NEEDRESCHED  = 1 << 1;
        /// Thread is a kernel-only thread.
        const TDF_KTHREAD      = 1 << 2;
    }
}

/// Thread lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Inactive,
    CanRun,
    Running,
    Sleeping,
    Stopped,
    Dead,
}

/// Mutable thread state protected by `Thread::inner`.
pub struct ThreadInner {
    pub state: ThreadState,
    pub flags: ThreadFlags,
    pub sigmask: SigSet,
    pub sigqueue: SigQueue,
    pub wchan: Option<usize>,
    pub wmesg: &'static str,
    pub name: [u8; 16],
    pub user_ctx: Option<UserContext>,
}

/// Thread Control Block (FreeBSD `struct thread`).
pub struct Thread {
    /// Unique thread identifier.
    pub tid: Tid,
    /// Weak back-reference to the containing Process. Prevents reference cycles.
    pub td_proc: Weak<Proc>,
    /// Underlying execution context provided by OSTD.
    pub td_kstack: Arc<Task>,
    /// Mutable thread state.
    pub inner: SpinLock<ThreadInner>,
}

impl Thread {
    /// Creates a new Thread wrapping an OSTD Task.
    pub fn new(tid: Tid, proc: &Arc<Proc>, task: Arc<Task>) -> Arc<Self> {
        let mut name = [0u8; 16];
        name[0] = b't';
        name[1] = b'h';
        name[2] = b'r';
        Arc::new(Self {
            tid,
            td_proc: Arc::downgrade(proc),
            td_kstack: task,
            inner: SpinLock::new(ThreadInner {
                state: ThreadState::CanRun,
                flags: ThreadFlags::empty(),
                sigmask: SigSet::empty(),
                sigqueue: SigQueue::new(),
                wchan: None,
                wmesg: "",
                name,
                user_ctx: None,
            }),
        })
    }

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
        let mut inner = self.inner.lock();
        if !inner.flags.contains(ThreadFlags::TDF_ASTPENDING) {
            return false;
        }

        let sigmask = inner.sigmask;
        // Check thread queue then process queue
        let thread_sig = inner.sigqueue.pop_unmasked(sigmask);
        drop(inner);

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
            true
        } else {
            self.inner.lock().flags.remove(ThreadFlags::TDF_ASTPENDING);
            false
        }
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
                // In full POSIX, set up signal frame on user stack and redirect rip.
                // For basic signal dispatch without user trampoline, log notice.
                ostd::info!("Signal {} handled by user handler", sig.as_u32());
            }
        }
    }

    /// Primary userspace execution loop for user threads.
    pub fn user_loop(&self, initial_ctx: UserContext) {
        let vmar = self.proc().expect("thread must belong to process").vmspace();
        vmar.activate();

        let mut user_mode = UserMode::new(initial_ctx);

        loop {
            // Check AST / pending signals prior to entering Ring 3
            if self.handle_ast() && self.inner.lock().state == ThreadState::Dead {
                return;
            }

            let return_reason = user_mode.execute(&DummyUserHooks);

            match return_reason {
                ReturnReason::UserSyscall => {
                    super::syscalls::dispatch(user_mode.context_mut());
                    if self.inner.lock().state == ThreadState::Dead {
                        return;
                    }
                }
                ReturnReason::UserException => {
                    if let Some(exception) = user_mode.context_mut().take_exception() {
                        match exception {
                            CpuException::PageFault(raw_pf) => {
                                let info = crate::vm::vmar::page_fault::PageFaultInfo::from_fault(
                                    raw_pf.addr,
                                    raw_pf.error_code,
                                );
                                if let Some(info) = info {
                                    if let Err(err) = vmar.handle_page_fault(&info) {
                                        ostd::error!(
                                            "Fatal user page fault at {:#x}: {:?}",
                                            raw_pf.addr,
                                            err
                                        );
                                        self.deliver_signal(Signal::SIGSEGV);
                                        if self.inner.lock().state == ThreadState::Dead {
                                            return;
                                        }
                                    }
                                } else {
                                    ostd::error!("User fault outside user range: {:#x}", raw_pf.addr);
                                    self.deliver_signal(Signal::SIGSEGV);
                                    if self.inner.lock().state == ThreadState::Dead {
                                        return;
                                    }
                                }
                            }
                            _ => {
                                ostd::error!("Unhandled user CPU exception: {:?}", exception);
                                self.deliver_signal(Signal::SIGILL);
                                if self.inner.lock().state == ThreadState::Dead {
                                    return;
                                }
                            }
                        }
                    }
                }
                ReturnReason::KernelEvent => {
                    // Preempted or kernel event occurred, loop back to check AST
                }
            }
        }
    }
}
