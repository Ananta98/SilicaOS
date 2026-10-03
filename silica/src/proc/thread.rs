// SPDX-License-Identifier: GPL-2.0

//! Thread management (FreeBSD `kern_thread.c`).
//!
//! A [`Thread`] represents an execution context within a [`Proc`]. It wraps an
//! underlying [`ostd::task::Task`], maintains user-space CPU contexts, signal masks,
//! and handles transitions between Ring 0 and Ring 3 via [`ostd::user::UserMode`].

use alloc::sync::{Arc, Weak};
use bitflags::bitflags;
use core::{
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};
use ostd::{
    arch::cpu::context::{CpuException, FsBase, UserContext},
    sync::SpinLock,
    task::{Task, TaskOptions},
    user::{ReturnReason, UserMode, UserModeHooks},
};

use super::{COMM_LEN, Proc};
use crate::{
    api::{
        errno::{Errno, Result},
        signal::{SigHandler, SigInfo, SigSet, SigStack, Signal, code, flags},
    },
    arch::signal::{self as sigframe, Redirect},
};
use super::signal::{QueuedSignal, SigQueue};

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
        /// get the thread to run again. See [`super::signal`].
        const TDF_ASTPENDING   = 1 << 0;
        /// Thread is a kernel-only thread.
        const TDF_KTHREAD      = 1 << 2;
        /// `SIGKILL` arrived while the thread was not running.
        ///
        /// OSTD cannot cancel a task, so this is a note that a thread which never
        /// runs again will never notice it was killed.
        const KILL_PENDING     = 1 << 3;
        /// The thread is inside a user-space handler.
        ///
        /// Set between entering a handler and `rt_sigreturn(2)`. It is what lets
        /// `rt_sigreturn` find the mask to put back, and what stops a stray
        /// `rt_sigreturn` from restoring a frame the thread never had.
        const IN_HANDLER       = 1 << 4;
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

/// Reports a pending AST to [`UserMode::execute`].
///
/// This is what makes signal delivery work at all. `UserModeHooks::has_kernel_event`
/// is called after every return into the kernel, so a thread that has a signal
/// waiting gets out of user space at the next opportunity instead of at its next
/// system call -- and a thread spinning in user space with a timer tick still gets
/// one, because interrupts reach `execute` too.
///
/// `DummyUserHooks` never reported anything, so the flag this watches was set and
/// then only ever read by the thread that set it.
pub struct ThreadHooks {
    /// `Weak`, because the hook is called from inside `execute` and must not keep
    /// the thread alive; the task itself holds the only strong reference.
    td: Weak<Thread>,
}

impl ThreadHooks {
    /// Hooks for the thread that owns `td`.
    pub fn new(td: &Arc<Thread>) -> Self {
        Self {
            td: Arc::downgrade(td),
        }
    }

    /// Hooks that never report an event.
    ///
    /// For a caller that holds only a `&Thread`. Signals will not reach that
    /// thread; only [`Self::new`] can deliver them.
    pub fn empty() -> Self {
        Self { td: Weak::new() }
    }
}

impl UserModeHooks for ThreadHooks {
    fn has_kernel_event(&self) -> bool {
        self.td
            .upgrade()
            .is_some_and(|td| td.inner.lock().flags.contains(ThreadFlags::TDF_ASTPENDING))
    }
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
    /// The thread's alternate signal stack (`sigaltstack(2)`).
    pub altstack: SigStack,
    /// The mask in force while a handler runs.
    ///
    /// `rt_sigreturn(2)` puts this back rather than the mask the signal was
    /// delivered under, because the handler ran with this one.
    pub handler_mask: SigSet,
    /// The base of the thread-local storage block, i.e. `IA32_FS_BASE`.
    ///
    /// Carried here rather than in `UserContext` because `ostd` has no field for
    /// it: its `RawUserContext` is the general registers, the trap number and the
    /// error code, and nothing else. `FsBase::save` and `load` bracket
    /// `UserMode::execute`, which is the only point the CPU is in user space.
    ///
    /// This is what makes `arch_prctl(ARCH_SET_FS)` possible, and without it no
    /// libc can start -- a C runtime reads its thread pointer out of `fs:0`
    /// before it does anything else.
    pub fs_base: usize,
    /// The signal that stopped this thread, if it is stopped.
    pub stopped_by: Option<Signal>,
    /// The address of the signal frame the thread is currently running inside, if
    /// it is in a handler.
    ///
    /// Recorded rather than reconstructed from the stack pointer when the handler
    /// returns: the frame may be on either stack, the handler may have moved the
    /// stack pointer anywhere, and a process that has been overwritten must not be
    /// able to choose what the kernel restores.
    pub handler_frame: Option<usize>,
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

    /// Queues a signal against this thread alone.
    ///
    /// For a signal aimed at one thread rather than at its process -- `tkill`,
    /// or a thread-directed internal fault.
    pub fn post_signal(&self, info: SigInfo) {
        self.inner
            .lock()
            .sigqueue
            .post(QueuedSignal { info });
        self.inner.lock().flags.insert(ThreadFlags::TDF_ASTPENDING);
    }

    /// Takes delivery of at most one pending signal.
    ///
    /// Returns `true` if a handler was entered, in which case the register file
    /// has been redirected and the next [`Self::user_loop`] iteration enters it.
    /// The thread's own queue is consulted before the process-wide one, because
    /// a thread-directed signal takes priority over one aimed at the process.
    pub fn handle_ast(&self) -> bool {
        if !self
            .inner
            .lock()
            .flags
            .contains(ThreadFlags::TDF_ASTPENDING)
        {
            return false;
        }

        let sigmask = self.inner.lock().sigmask;
        let queued = self.inner.lock().sigqueue.take_unmasked(&sigmask).or_else(|| {
            self.proc()
                .and_then(|proc| proc.sigqueue.take_unmasked(&sigmask))
        });

        let Some(queued) = queued else {
            // Nothing was deliverable, but the trap only stands down once both
            // queues are genuinely empty: a signal that is merely masked is
            // deferred, not lost, and clearing the flag here would strand it with
            // nothing left to re-arm.
            let thread_empty = self.inner.lock().sigqueue.is_empty();
            let proc_empty = self
                .proc()
                .is_none_or(|proc| proc.sigqueue.is_empty());
            if thread_empty && proc_empty {
                self.inner.lock().flags.remove(ThreadFlags::TDF_ASTPENDING);
            }
            return false;
        };

        self.deliver_signal(queued)
    }

    /// Applies the disposition for one signal.
    ///
    /// Returns `true` if a handler was entered.
    fn deliver_signal(&self, queued: QueuedSignal) -> bool {
        let sig = queued.signal();
        let Some(proc) = self.proc() else {
            return false;
        };

        let action = proc.sigacts.lock().get(sig);
        match action.sa_handler {
            SigHandler::Ignore => {}
            SigHandler::Handler(_) => {
                let Some((frame_addr, redirect)) = self.build_handler_frame(sig, &queued.info, action)
                else {
                    // No frame could be built. POSIX does not say what to do, and
                    // killing the process here would turn a full stack into a
                    // signal loop, so the signal is discarded and logged.
                    ostd::error!(
                        "proc: discarding {sig}: no room for a signal frame on tid {}",
                        self.tid
                    );
                    return false;
                };
                self.enter_handler(sig, action, frame_addr, redirect);
                return true;
            }
            SigHandler::Default => {
                super::signal::run_default_action(self, sig);
            }
        }

        // The default action may have terminated the thread; the caller checks.
        false
    }

    /// Builds the frame for a handler and applies the mask it runs under.
    ///
    /// Returns `None` when no frame can be built.
    fn build_handler_frame(
        &self,
        sig: Signal,
        info: &SigInfo,
        action: crate::api::signal::SigAction,
    ) -> Option<(usize, Redirect)> {
        let proc = self.proc()?;
        let vmar = proc.vmspace();

        // The context is cloned rather than borrowed: the lock cannot be held
        // across `build_frame`, which writes into the address space and can fault.
        let (ctx, saved_mask, altstack) = {
            let td_inner = self.inner.lock();
            (
                td_inner.user_ctx.clone()?,
                td_inner.sigmask,
                td_inner.altstack,
            )
        };
        let on_stack = action.sa_flags & flags::SA_ONSTACK != 0;

        let frame_addr =
            sigframe::build_frame(&vmar, &ctx, info, &saved_mask, &altstack, on_stack).ok()?;

        Some((frame_addr, if action.sa_flags & flags::SA_SIGINFO != 0 {
            Redirect::with_siginfo(action.sa_handler_addr(), sig, frame_addr)
        } else {
            Redirect::plain(action.sa_handler_addr(), sig, frame_addr)
        }))
    }

    /// Points the register file at a handler and puts the thread into the state
    /// it needs to come back.
    fn enter_handler(
        &self,
        sig: Signal,
        action: crate::api::signal::SigAction,
        frame_addr: usize,
        redirect: Redirect,
    ) {
        {
            let mut inner = self.inner.lock();
            if let Some(ctx) = inner.user_ctx.as_mut() {
                redirect.apply(ctx);
            }
            inner.handler_frame = Some(frame_addr);
            // The mask the handler runs under, kept so that `rt_sigreturn` can
            // put back what was in force *before* the handler changed anything.
            inner.handler_mask = action.effective_mask(sig);
            inner.sigmask = inner.handler_mask;
            inner.flags.insert(ThreadFlags::IN_HANDLER);
            inner.flags.remove(ThreadFlags::TDF_ASTPENDING);
        }

        // `SA_RESETHAND` restores the default for one-shot handlers.
        if action.sa_flags & flags::SA_RESETHAND != 0
            && let Some(proc) = self.proc()
        {
            proc.sigacts.lock().set(sig, crate::api::signal::SigAction::default());
        }
    }

    /// Returns the thread to user space from a handler.
    ///
    /// Restores the register file from the `ucontext` the handler left behind and
    /// puts the pre-handler mask back.
    ///
    /// The restore goes into `ctx`, which is the register file about to be
    /// executed. That is deliberately not this thread's copy of it: `UserMode`
    /// holds its own, and `rt_sigreturn` is reached from *inside*
    /// `UserMode::execute`, so writing to the thread's copy would be undone by
    /// whatever ran next.
    ///
    /// The frame is the one the kernel recorded when the handler was entered, not
    /// one the caller names, so a process that has been overwritten in the meantime
    /// cannot choose what gets restored.
    pub fn return_from_handler(&self, ctx: &mut UserContext) -> Result<()> {
        let proc = self.proc().ok_or(Errno::ESRCH)?;
        let vmar = proc.vmspace();

        let frame_addr = self.inner.lock().handler_frame.ok_or(Errno::EFAULT)?;
        let frame = sigframe::read_frame(&vmar, frame_addr)?;
        // The mask comes out of the frame, so a handler that changed its own mask
        // and then returned still gets the right one back.
        let restored_mask = frame.uc.uc_sigmask;
        let restore = sigframe::Restore::from_frame(&frame, &vmar)?;
        restore.apply(ctx);

        let mut inner = self.inner.lock();
        inner.sigmask = restored_mask;
        inner.handler_mask = SigSet::empty();
        inner.handler_frame = None;
        inner.flags.remove(ThreadFlags::IN_HANDLER);
        Ok(())
    }

    /// The thread's TLS base, i.e. the value `fs:` resolves to.
    pub fn fs_base(&self) -> usize {
        self.inner.lock().fs_base
    }

    /// Sets the thread's TLS base.
    pub fn set_fs_base(&self, base: usize) {
        self.inner.lock().fs_base = base;
    }

    /// Primary userspace execution loop for user threads.
    ///
    /// Returns when the thread dies, or when its process has been collected and
    /// `Self::proc` can no longer be resolved.
    pub fn user_loop(&self, initial_ctx: UserContext, hooks: ThreadHooks) {
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
            // Adopt whatever the register file says now. `UserMode` keeps its own
            // copy of it, and the last thing that happened there was a syscall or a
            // fault, so the authoritative version is the one it holds -- not the
            // one taken when the thread was built.
            self.adopt_ctx(user_mode.context());

            // Check AST / pending signals prior to entering Ring 3
            self.handle_ast();
            if self.is_dead() || self.is_stopped() {
                return;
            }

            // Hand back whatever the signal path changed -- the redirect into a
            // handler, or the frame a handler returned through. Without this the
            // register file `UserMode` runs is never the one the signal code just
            // wrote, and entering a handler would silently do nothing.
            self.publish_ctx(user_mode.context_mut());

            // TLS lives in `IA32_FS_BASE`, which `ostd` does not carry per task.
            // `UserMode::execute` is the only point at which the CPU is in user
            // space, so this is the only point at which it can be swapped.
            FsBase::new(self.fs_base()).load();

            let return_reason = user_mode.execute(&hooks);

            // Read it back before anything else can overwrite the register: the
            // next `execute` may be on a different thread.
            let mut saved = FsBase::new(0);
            saved.save();
            self.set_fs_base(saved.addr());

            match return_reason {
                ReturnReason::UserSyscall => {
                    crate::syscall::dispatch(user_mode.context_mut());
                    // A handler was entered or left through the syscall path.
                    self.adopt_ctx(user_mode.context());
                }
                ReturnReason::UserException => {
                    if let Some(exception) = user_mode.context_mut().take_exception() {
                        match exception {
                            CpuException::PageFault(raw_pf) => {
                                let info = crate::vm::vmar::page_fault::PageFaultInfo::from_fault(
                                    raw_pf.addr,
                                    raw_pf.error_code,
                                );
                                let unresolved = match info {
                                    // The address is outside the range user space
                                    // may touch at all, so no mapping can help.
                                    None => {
                                        ostd::error!(
                                            "User fault outside user range: {:#x}",
                                            raw_pf.addr
                                        );
                                        Some(code::SEGV_MAPERR)
                                    }
                                    Some(info) => match vmar.handle_page_fault(&info) {
                                        Ok(()) => None,
                                        Err(err) => {
                                            ostd::error!(
                                                "Fatal user page fault at {:#x}: {err:?}",
                                                raw_pf.addr
                                            );
                                            Some(code::SEGV_ACCERR)
                                        }
                                    },
                                };
                                if let Some(si_code) = unresolved {
                                    // The address goes into the `siginfo_t`, which
                                    // is the only way a handler learns where it
                                    // faulted.
                                    let queued = QueuedSignal {
                                        info: SigInfo::fault(
                                            Signal::SIGSEGV,
                                            si_code,
                                            raw_pf.addr,
                                        ),
                                    };
                                    self.deliver_signal(queued);
                                }
                            }
                            // TODO: map every exception to the signal POSIX
                            // requires. This reports SIGILL for all of them, so a
                            // divide-by-zero or a general-protection fault is
                            // indistinguishable from a bad opcode.
                            other => {
                                let sig = signal_for_exception(&other);
                                ostd::error!("Unhandled user CPU exception: {other:?} -> {sig}");
                                self.deliver_signal(QueuedSignal {
                                    info: SigInfo::kernel(sig, code::KERNEL),
                                });
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

    /// Copies the register file out of `UserMode` and into this thread.
    fn adopt_ctx(&self, from: &UserContext) {
        self.inner.lock().user_ctx = Some(from.clone());
    }

    /// Copies this thread's register file into `UserMode`.
    fn publish_ctx(&self, into: &mut UserContext) {
        if let Some(ctx) = self.inner.lock().user_ctx.as_ref() {
            *into = ctx.clone();
        }
    }

    /// Returns whether this thread has been terminated.
    fn is_dead(&self) -> bool {
        self.inner.lock().state == ThreadState::Dead
    }

    /// Returns whether this thread is stopped by a job-control signal.
    fn is_stopped(&self) -> bool {
        self.inner.lock().state == ThreadState::Stopped
    }
}

/// The signal a CPU exception raises when it reaches user space.
///
/// Getting this wrong is not cosmetic: a handler that tests for `SIGILL` when the
/// fault was a bad address would take the wrong recovery, and `SIGSEGV` is the
/// one that carries the address in its `siginfo_t`.
fn signal_for_exception(exception: &CpuException) -> Signal {
    match exception {
        // A bad instruction.
        CpuException::InvalidOpcode => Signal::SIGILL,
        // Arithmetic. The codes that say which fault it was are in the
        // `siginfo_t`, which this path cannot fill in.
        CpuException::DivisionError | CpuException::X87FloatingPointException | CpuException::DeviceNotAvailable => Signal::SIGFPE,
        // A misaligned access.
        CpuException::AlignmentCheck | CpuException::GeneralProtectionFault(_) => Signal::SIGBUS,
        // The trap instruction and the debugger breakpoint.
        CpuException::BreakPoint | CpuException::Debug => Signal::SIGTRAP,
        // Everything address-related: the page fault handler in `user_loop` deals
        // with those before they get here.
        CpuException::PageFault(_)
        | CpuException::SegmentNotPresent(_)
        | CpuException::StackSegmentFault(_)
        | CpuException::Overflow
        | CpuException::BoundRangeExceeded => Signal::SIGSEGV,
        // Reserved vectors are traps from user space, which are how `int $n` and
        // the `sysenter` family behave.
        _ => Signal::SIGSYS,
    }
}
