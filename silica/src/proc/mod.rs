// SPDX-License-Identifier: GPL-2.0

//! Process management subsystem (FreeBSD `sys/proc.h`, `kern_proc.c`).
//!
//! Provides the process control block ([`Proc`]), lifecycle state machine,
//! address space bindings, credentials, and hierarchy relationships.

pub mod cred;
pub mod limit;
pub mod signal;
pub mod tree;
pub mod thread;
pub mod exit;
pub mod wait;
pub mod kthread;
pub mod fork;
pub mod exec;
pub mod stack;
pub mod init;

use alloc::{string::String, sync::Arc, vec::Vec};
use ostd::sync::SpinLock;

use crate::{
    errno::Result,
    vm::vmar::Vmar,
};
use self::{
    cred::Ucred,
    limit::Plimit,
    signal::{SigActs, SigQueue, Signal},
    thread::{Thread, Tid},
    tree::Pid,
};
use crate::fs::fd::Filedesc;

/// Process lifecycle state machine mirroring FreeBSD `p_state` / `PRS_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Process is being created, resources are being allocated.
    New,
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

impl ExitStatus {
    /// Formats as standard POSIX wait status integer.
    pub fn as_raw(&self) -> i32 {
        match *self {
            ExitStatus::Exited(code) => (code & 0xff) << 8,
            ExitStatus::Signaled { signal, core_dumped } => {
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
    pub reaper: Option<Pid>,
    pub children: Vec<Pid>,
    pub threads: Vec<Arc<Thread>>,
    pub xstat: Option<ExitStatus>,
    pub comm: [u8; 16],
    pub is_reaper: bool,
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
    pub sigqueue: SpinLock<SigQueue>,
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
        let mut comm = [0u8; 16];
        let bytes = comm_name.as_bytes();
        let copy_len = bytes.len().min(15);
        comm[..copy_len].copy_from_slice(&bytes[..copy_len]);

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
                comm,
                is_reaper,
            }),
            vmspace: SpinLock::new(vmspace),
            fd_table,
            cred: SpinLock::new(cred),
            limit: SpinLock::new(limit),
            sigacts,
            sigqueue: SpinLock::new(SigQueue::new()),
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

    /// Posts a signal to the process and alerts threads.
    pub fn post_signal(&self, signal: Signal) {
        // Discard if ignored
        if self.sigacts.lock().get(signal).sa_handler == signal::SigHandler::Ignore {
            return;
        }

        self.sigqueue.lock().post(signal);

        // Find a thread to wake or notify
        let inner = self.inner.lock();
        for thread in inner.threads.iter() {
            let mut td_inner = thread.inner.lock();
            if !td_inner.sigmask.contains(signal) {
                td_inner.flags.insert(thread::ThreadFlags::TDF_ASTPENDING);
                return;
            }
        }

        // If all threads mask it, set AST pending on all threads anyway
        for thread in inner.threads.iter() {
            thread.inner.lock().flags.insert(thread::ThreadFlags::TDF_ASTPENDING);
        }
    }

    /// Returns the command name.
    pub fn comm_name(&self) -> String {
        let inner = self.inner.lock();
        let len = inner.comm.iter().position(|&b| b == 0).unwrap_or(inner.comm.len());
        String::from_utf8_lossy(&inner.comm[..len]).into_owned()
    }

    /// Returns the active address space.
    pub fn vmar(&self) -> Arc<Vmar> {
        self.vmspace()
    }
}

/// Convenience module for ELF loader operations.
pub mod elf {
    pub use super::exec::{load_and_setup, load_elf, setup_user_stack};
}

/// Creates the initial user process (`init`, PID 1).
pub fn create_init_process() -> Result<Arc<Proc>> {
    create_init_process_with_vmar(Vmar::new())
}

/// Creates the initial user process (`init`, PID 1) with a given address space.
pub fn create_init_process_with_vmar(vmar: Arc<Vmar>) -> Result<Arc<Proc>> {
    let pid = tree::PID_INIT;
    let _ = tree::PID_ALLOCATOR.lock().reserve(pid);
    let fd_table = Arc::new(SpinLock::new(Filedesc::new()));
    let cred = Arc::new(Ucred::root());
    let limit = Arc::new(Plimit::default_limits());
    let sigacts = Arc::new(SpinLock::new(SigActs::new()));

    let proc = Proc::new(
        pid,
        None,
        vmar,
        fd_table,
        cred,
        limit,
        sigacts,
        "init",
    );

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

    let mut name_buf = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(15);
    name_buf[..len].copy_from_slice(&bytes[..len]);

    let thread = Arc::new_cyclic(|weak_thread: &alloc::sync::Weak<Thread>| {
        let weak_for_closure = weak_thread.clone();
        let weak_for_task = weak_thread.clone();
        let user_ctx_clone = user_ctx.clone();
        let options = ostd::task::TaskOptions::new(move || {
            if let Some(td) = weak_for_closure.upgrade() {
                td.user_loop(user_ctx_clone);
            }
        })
        .data(weak_for_task)
        .local_data(vmar);

        let task = crate::sched::build(options, nice).expect("failed to build user task");

        Thread {
            tid,
            td_proc: proc_weak,
            td_kstack: task,
            inner: SpinLock::new(thread::ThreadInner {
                state: thread::ThreadState::CanRun,
                flags: thread::ThreadFlags::empty(),
                sigmask: signal::SigSet::empty(),
                sigqueue: signal::SigQueue::new(),
                wchan: None,
                wmesg: "",
                name: name_buf,
                user_ctx: Some(user_ctx),
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

/// Creates a new user process.
pub fn create_process(name: &str) -> Result<Arc<Proc>> {
    let pid = tree::PID_ALLOCATOR.lock().allocate()?;
    let vmar = Vmar::new();
    let fd_table = Arc::new(SpinLock::new(Filedesc::new()));
    let cred = Arc::new(Ucred::root());
    let limit = Arc::new(Plimit::default_limits());
    let sigacts = Arc::new(SpinLock::new(SigActs::new()));

    let proc = Proc::new(
        pid,
        None,
        vmar,
        fd_table,
        cred,
        limit,
        sigacts,
        name,
    );

    tree::allproc_insert(Arc::clone(&proc));
    Ok(proc)
}

/// Forks a process structure and address space.
pub fn fork_process(parent: &Arc<Proc>) -> Result<Arc<Proc>> {
    let child_pid = tree::PID_ALLOCATOR.lock().allocate()?;
    let parent_vmar = parent.vmspace();
    let child_vmar = Vmar::fork_from(&parent_vmar);
    let child_fd = Arc::new(SpinLock::new(parent.fd_table.lock().clone_table()));
    let child_cred = parent.cred();
    let child_limit = parent.limit();
    let child_sigacts = Arc::new(SpinLock::new(signal::SigActs {
        actions: parent.sigacts.lock().actions,
    }));

    let child = Proc::new(
        child_pid,
        Some(parent.pid),
        child_vmar,
        child_fd,
        child_cred,
        child_limit,
        child_sigacts,
        &parent.comm_name(),
    );

    parent.inner.lock().children.push(child_pid);
    tree::allproc_insert(Arc::clone(&child));
    Ok(child)
}

/// Terminates a process with the given exit code.
pub fn exit_process(proc: &Arc<Proc>, code: i32) {
    if let Some(td) = proc.main_thread() {
        exit::exit1(&td, ExitStatus::Exited(code));
    } else {
        let mut inner = proc.inner.lock();
        inner.state = ProcState::Zombie;
        inner.xstat = Some(ExitStatus::Exited(code));
        let ppid = inner.ppid;
        drop(inner);

        if let Some(parent) = ppid.and_then(tree::allproc_find) {
            parent.post_signal(signal::Signal::SIGCHLD);
            wait::notify_waiters(parent.pid);
        }
    }
}

/// Waits for a specific child process to change state.
pub fn waitpid(parent: &Arc<Proc>, target_pid: Pid, _options: u32) -> Result<wait::WaitResult> {
    let res = wait::kern_wait6(parent, wait::IdType::Pid(target_pid), wait::WaitOptions::WEXITED)?;
    res.ok_or(crate::errno::Errno::ECHILD)
}