// SPDX-License-Identifier: GPL-2.0

//! Process cloning via copy-on-write fork (FreeBSD `kern_fork.c`).
//!
//! Implements FreeBSD `fork1` which clones the calling process, duplicates its
//! address space using copy-on-write semantics via [`Vmar::fork_from`], duplicates
//! open file descriptors, and starts the child execution thread.

use alloc::sync::{Arc, Weak};
use ostd::{
    arch::cpu::context::UserContext,
    sync::SpinLock,
    task::TaskOptions,
};

use crate::{
    errno::{Errno, Result},
    proc::{
        Proc,
        thread::{Thread, ThreadFlags, ThreadInner, ThreadState, alloc_tid},
        tree::{PID_ALLOCATOR, Pid, allproc_insert},
    },
    vm::vmar::Vmar,
};

/// FreeBSD `fork1`: clones calling process into a new child process with CoW memory.
pub fn fork1(caller_td: &Thread, caller_user_ctx: Option<&UserContext>) -> Result<Pid> {
    let caller_proc = caller_td.proc().ok_or(Errno::ESRCH)?;

    // 1. Allocate child PID
    let child_pid = PID_ALLOCATOR.lock().allocate()?;

    // 2. Clone address space with Copy-on-Write pages
    let parent_vmar = caller_proc.vmspace();
    let child_vmar = Vmar::fork_from(&parent_vmar);

    // 3. Clone file descriptor table
    let child_fd_table = Arc::new(SpinLock::new(
        caller_proc.fd_table.lock().clone_table(),
    ));

    // 4. Share credentials and limits (Copy-on-Write)
    let child_cred = caller_proc.cred();
    let child_limit = caller_proc.limit();
    let child_sigacts = Arc::new(SpinLock::new(
        super::signal::SigActs {
            actions: caller_proc.sigacts.lock().actions,
        },
    ));

    let comm = caller_proc.comm_name();

    // 5. Create child process PCB
    let child_proc = Proc::new(
        child_pid,
        Some(caller_proc.pid),
        Arc::clone(&child_vmar),
        child_fd_table,
        child_cred,
        child_limit,
        child_sigacts,
        &comm,
    );

    // 6. Set up child's user context
    let mut child_ctx = caller_user_ctx.cloned().unwrap_or_default();
    // In child, fork returns 0 in rax, and carry flag is cleared
    child_ctx.general_regs_mut().rax = 0;
    child_ctx.general_regs_mut().rflags &= !(1 << 0);

    // 7. Create child thread & task
    let child_tid = alloc_tid();
    let child_proc_weak = Arc::downgrade(&child_proc);
    let child_vmar_clone = Arc::clone(&child_vmar);

    let child_thread = Arc::new_cyclic(|weak_thread: &Weak<Thread>| {
        let weak_td_clone = weak_thread.clone();
        let child_ctx_clone = child_ctx.clone();

        let options = TaskOptions::new(move || {
            if let Some(td) = weak_td_clone.upgrade() {
                td.user_loop(child_ctx_clone);
            }
        })
        .data(weak_thread.clone())
        .local_data(child_vmar_clone);

        let task = super::thread::build_task(options, 0);

        let caller_td_inner = caller_td.inner.lock();
        Thread {
            tid: child_tid,
            td_proc: child_proc_weak,
            td_kstack: task,
            inner: SpinLock::new(ThreadInner {
                state: ThreadState::CanRun,
                flags: ThreadFlags::empty(),
                sigmask: caller_td_inner.sigmask,
                sigqueue: super::signal::SigQueue::new(),
                wchan: None,
                wmesg: "",
                name: caller_td_inner.name,
                user_ctx: Some(child_ctx),
            }),
        }
    });

    child_proc.add_thread(Arc::clone(&child_thread));

    // 8. Register in process tree
    allproc_insert(Arc::clone(&child_proc));
    caller_proc.inner.lock().children.push(child_pid);

    // 9. Enqueue child task to run
    child_thread.run();

    Ok(child_pid)
}
