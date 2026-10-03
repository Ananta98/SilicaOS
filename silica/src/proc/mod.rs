// SPDX-License-Identifier: GPL-2.0

//! Process management subsystem (FreeBSD `sys/proc.h`, `kern_proc.c`).
//!
//! Provides the process control block ([`Proc`]), lifecycle state machine,
//! address space bindings, and hierarchy relationships.
//!
//! [`Proc`] holds no lifecycle logic of its own beyond the accessors that keep
//! its fields coherent; the operations on a process live in the modules that
//! mirror the FreeBSD kernel file they come from ([`fork`], [`exec`], [`exit`],
//! [`wait`], [`kthread`], [`init`]). [`tree`] owns the global process table and
//! the lock order that goes with it.
//!
//! The shapes a process exposes to user space are not here: credentials,
//! resource limits and signal dispositions are ABI, and belong in [`crate::api`]
//! alongside their syscall definitions. This module uses them.

pub mod exec;
pub mod exit;
pub mod fork;
pub mod init;
pub mod job_control;
pub mod kthread;
pub mod signal;
pub mod thread;
pub mod tree;
pub mod wait;

use alloc::{string::String, sync::Arc, vec::Vec};
use ostd::sync::SpinLock;

use self::{
    signal::SigQueue,
    thread::{Thread, Tid},
    tree::Pid,
};
use crate::{
    api::{
        cred::Ucred,
        errno::Result,
        limit::Plimit,
        signal::{SigActs, SigInfo, SigSet, SigStack, Signal},
    },
    fs::fd::Filedesc,
    vm::vmar::Vmar,
};

/// The width of a process command name, including the NUL terminator that
/// terminates a shorter name.
const COMM_LEN: usize = 16;

/// Builds a NUL-padded command name from `name`.
///
/// Truncation keeps the trailing NUL, so the result is always a valid C string
/// when read up to the first NUL by [`Proc::comm_name`].
fn comm_from_name(name: &str) -> [u8; COMM_LEN] {
    let mut comm = [0u8; COMM_LEN];
    let len = name.len().min(COMM_LEN - 1);
    comm[..len].copy_from_slice(&name.as_bytes()[..len]);
    comm
}

/// Process lifecycle state machine mirroring FreeBSD `p_state` / `PRS_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Process is alive and actively running or sleeping (FreeBSD PRS_NORMAL).
    Alive,
    /// Process has terminated via `exit1` and is waiting for parent/reaper to reap (FreeBSD PRS_ZOMBIE).
    Zombie,
}

/// Process exit reason and status payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// Terminated normally via exit(code). Code is 0..=255.
    Exited(i32),
    /// Terminated abnormally due to an unhandled signal.
    Signaled { signal: Signal, core_dumped: bool },
    /// Stopped by a signal (e.g., SIGSTOP, SIGTSTP).
    Stopped(Signal),
    /// Continued by SIGCONT.
    Continued,
}

impl core::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exited(code) => write!(f, "exit code {code}"),
            Self::Signaled { signal, core_dumped } => {
                write!(f, "signal {} (core {})", signal.as_u32(), u8::from(*core_dumped))
            }
            Self::Stopped(signal) => write!(f, "stopped by signal {}", signal.as_u32()),
            Self::Continued => f.write_str("continued"),
        }
    }
}

impl ExitStatus {
    /// Formats as standard POSIX wait status integer.
    pub fn as_raw(&self) -> i32 {
        match *self {
            ExitStatus::Exited(code) => (code & 0xff) << 8,
            ExitStatus::Signaled {
                signal,
                core_dumped,
            } => {
                let sig = signal.as_u32() as i32 & 0x7f;
                let core = if core_dumped { 0x80 } else { 0 };
                sig | core
            }
            ExitStatus::Stopped(signal) => ((signal.as_u32() as i32 & 0xff) << 8) | 0x7f,
            ExitStatus::Continued => 0xffff,
        }
    }
}

/// Mutable process state protected by `Proc::inner` SpinLock.
pub struct ProcInner {
    pub state: ProcState,
    pub ppid: Option<Pid>,
    /// An explicit reaper for orphaned children.
    ///
    /// TODO: subreaper support. Nothing assigns this yet, so it is always `None`
    /// and [`exit::exit1`] always resolves the reaper by walking the ancestor
    /// chain in [`tree::tree_find_reaper`].
    pub reaper: Option<Pid>,
    pub children: Vec<Pid>,
    pub threads: Vec<Arc<Thread>>,
    pub xstat: Option<ExitStatus>,
    pub comm: [u8; COMM_LEN],
    pub is_reaper: bool,
    /// The process group this process belongs to, if it has joined one.
    pub pgid: Option<Pid>,
    /// Set while the process is stopped, and cleared when it resumes.
    ///
    /// Distinct from [`Self::state`]: a stopped process is not a zombie and will
    /// run again, which is exactly what a zombie will not do.
    pub stop_status: Option<ExitStatus>,
}

/// Process Control Block (mirroring FreeBSD `struct proc`).
pub struct Proc {
    /// Unique process identifier. Immutable.
    pub pid: Pid,

    /// Mutable lifecycle fields protected by spinlock.
    pub inner: SpinLock<ProcInner>,

    /// Address space descriptor. Replaced atomically during execve.
    pub vmspace: SpinLock<Arc<Vmar>>,

    /// Open file descriptor table. Shared across threads, cloned across fork.
    pub fd_table: Arc<SpinLock<Filedesc>>,

    /// Security credentials. Immutable, replaced via Copy-on-Write.
    pub cred: SpinLock<Arc<Ucred>>,

    /// Resource limits. Replaced via Copy-on-Write.
    pub limit: SpinLock<Arc<Plimit>>,

    /// Signal dispositions (shared by all threads in this process).
    pub sigacts: Arc<SpinLock<SigActs>>,

    /// Process-wide pending signal queue.
    pub sigqueue: SigQueue,
}

impl Proc {
    /// Constructs a new Process Control Block.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pid: Pid,
        ppid: Option<Pid>,
        vmspace: Arc<Vmar>,
        fd_table: Arc<SpinLock<Filedesc>>,
        cred: Arc<Ucred>,
        limit: Arc<Plimit>,
        sigacts: Arc<SpinLock<SigActs>>,
        comm_name: &str,
    ) -> Arc<Self> {
        let is_reaper = pid == tree::PID_INIT;

        Arc::new(Self {
            pid,
            inner: SpinLock::new(ProcInner {
                state: ProcState::Alive,
                ppid,
                reaper: None,
                children: Vec::new(),
                threads: Vec::new(),
                xstat: None,
                comm: comm_from_name(comm_name),
                is_reaper,
                pgid: None,
                stop_status: None,
            }),
            vmspace: SpinLock::new(vmspace),
            fd_table,
            cred: SpinLock::new(cred),
            limit: SpinLock::new(limit),
            sigacts,
            sigqueue: SigQueue::new(),
        })
    }

    /// Returns the unique PID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Returns the process lifecycle state.
    pub fn state(&self) -> ProcState {
        self.inner.lock().state
    }

    /// Returns the active address space.
    pub fn vmspace(&self) -> Arc<Vmar> {
        Arc::clone(&*self.vmspace.lock())
    }

    /// Replaces the address space (used during `execve`).
    pub fn set_vmspace(&self, new_vmar: Arc<Vmar>) {
        *self.vmspace.lock() = new_vmar;
    }

    /// Returns current credentials.
    pub fn cred(&self) -> Arc<Ucred> {
        Arc::clone(&*self.cred.lock())
    }

    /// Atomically replaces credentials (Copy-on-Write).
    pub fn set_cred(&self, new_cred: Arc<Ucred>) {
        *self.cred.lock() = new_cred;
    }

    /// Returns current resource limits.
    pub fn limit(&self) -> Arc<Plimit> {
        Arc::clone(&*self.limit.lock())
    }

    /// Atomically replaces resource limits (Copy-on-Write).
    pub fn set_limit(&self, new_limit: Arc<Plimit>) {
        *self.limit.lock() = new_limit;
    }

    /// Adds a thread to this process.
    pub fn add_thread(&self, thread: Arc<Thread>) {
        let mut inner = self.inner.lock();
        inner.threads.push(thread);
    }

    /// Removes a thread from this process by TID.
    pub fn remove_thread(&self, tid: Tid) {
        let mut inner = self.inner.lock();
        inner.threads.retain(|td| td.tid != tid);
    }

    /// Returns the primary (first) thread of this process.
    pub fn main_thread(&self) -> Option<Arc<Thread>> {
        let inner = self.inner.lock();
        inner.threads.first().cloned()
    }

    /// Sends a signal to this process, from the kernel.
    ///
    /// A convenience over [`Proc::send_signal`] for the callers inside the kernel
    /// that do not have a `siginfo_t` of their own. Everything interesting --
    /// queueing, choosing a thread, deciding the disposition -- happens there.
    pub fn post_signal(&self, sig: Signal) {
        let info = SigInfo::kernel(sig, crate::api::signal::code::KERNEL);
        if let Err(err) = self.send_signal(info, 0) {
            ostd::debug!("proc: {sig} to pid {} was not delivered: {err}", self.pid);
        }
    }

    /// Returns the command name.
    pub fn comm_name(&self) -> String {
        let inner = self.inner.lock();
        let len = inner
            .comm
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(inner.comm.len());
        String::from_utf8_lossy(&inner.comm[..len]).into_owned()
    }
}

/// Creates the initial user process (`init`, PID 1) with a given address space.
///
/// PID 1 is reserved here rather than on a separate path: `PidAllocator` hands
/// out any unallocated number starting from 1, so unless PID 1 is marked
/// allocated before the first `allocate()`, init's own identifier can be given
/// to some later process.
pub fn create_init_process_with_vmar(vmar: Arc<Vmar>) -> Result<Arc<Proc>> {
    let pid = tree::PID_INIT;
    tree::PID_ALLOCATOR.lock().reserve(pid)?;

    let fd_table = Arc::new(SpinLock::new(Filedesc::with_stdio()));
    let cred = Arc::new(Ucred::root());
    let limit = Arc::new(Plimit::default_limits());
    let sigacts = Arc::new(SpinLock::new(SigActs::new()));

    let proc = Proc::new(pid, None, vmar, fd_table, cred, limit, sigacts, "init");

    tree::allproc_insert(Arc::clone(&proc));
    Ok(proc)
}

/// Creates a user thread belonging to `proc`.
pub fn create_user_thread(
    proc: &Arc<Proc>,
    name: &str,
    user_ctx: ostd::arch::cpu::context::UserContext,
    nice: i8,
) -> Result<Arc<Thread>> {
    let tid = thread::alloc_tid();
    let vmar = proc.vmspace();
    let proc_weak = Arc::downgrade(proc);
    let name_buf = comm_from_name(name);

    let thread = Arc::new_cyclic(|weak_thread: &alloc::sync::Weak<Thread>| {
        let weak_for_closure = weak_thread.clone();
        let weak_for_task = weak_thread.clone();
        let user_ctx_clone = user_ctx.clone();
        let options = ostd::task::TaskOptions::new(move || {
            if let Some(td) = weak_for_closure.upgrade() {
                let hooks = thread::ThreadHooks::new(&td);
                td.user_loop(user_ctx_clone, hooks);
            }
        })
        .data(weak_for_task)
        .local_data(vmar);

        let task = thread::build_task(options, nice);

        Thread {
            tid,
            td_proc: proc_weak,
            td_kstack: task,
            inner: SpinLock::new(thread::ThreadInner {
                state: thread::ThreadState::CanRun,
                flags: thread::ThreadFlags::empty(),
                sigmask: SigSet::initial(),
                sigqueue: SigQueue::new(),
                wchan: None,
                wmesg: "",
                name: name_buf,
                user_ctx: Some(user_ctx),
                altstack: SigStack::default(),
                handler_mask: SigSet::empty(),
                fs_base: 0,
                stopped_by: None,
                handler_frame: None,
            }),
        }
    });

    proc.add_thread(Arc::clone(&thread));
    Ok(thread)
}

/// Creates the main thread for `proc`.
pub fn create_main_thread(
    proc: &Arc<Proc>,
    user_ctx: ostd::arch::cpu::context::UserContext,
) -> Result<Arc<Thread>> {
    create_user_thread(proc, "init", user_ctx, 0)
}
