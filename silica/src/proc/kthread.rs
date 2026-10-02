// SPDX-License-Identifier: GPL-2.0

//! Kernel processes and threads (FreeBSD `kern_kthread.c`).
//!
//! Provides infrastructure for creating kernel-space processes (`kproc_create`)
//! and kernel-space background threads (`kthread_add`).

use alloc::sync::{Arc, Weak};
use ostd::{sync::SpinLock, task::TaskOptions};

use crate::{
    errno::Result,
    proc::{
        Proc,
        cred::Ucred,
        limit::Plimit,
        signal::SigActs,
        thread::{Thread, ThreadFlags, ThreadInner, ThreadState, alloc_tid},
        tree::{PID_ALLOCATOR, allproc_insert},
    },
    vm::vmar::Vmar,
};
use crate::fs::fd::Filedesc;

/// Spawns a dedicated kernel process (e.g. `pagedaemon`, `bufdaemon`).
pub fn kproc_create(
    name: &str,
    entry: fn(),
    nice: i8,
) -> Result<Arc<Proc>> {
    let pid = PID_ALLOCATOR.lock().allocate()?;
    let vmar = Vmar::new();
    let fd_table = Arc::new(SpinLock::new(Filedesc::new()));
    let cred = Arc::new(Ucred::root());
    let limit = Arc::new(Plimit::default_limits());
    let sigacts = Arc::new(SpinLock::new(SigActs::new()));

    let proc = Proc::new(
        pid,
        None,
        Arc::clone(&vmar),
        fd_table,
        cred,
        limit,
        sigacts,
        name,
    );

    allproc_insert(Arc::clone(&proc));

    let _thread = kthread_add(&proc, name, entry, nice)?;
    Ok(proc)
}

/// Adds and starts a kernel thread belonging to a kernel process.
pub fn kthread_add(
    proc: &Arc<Proc>,
    name: &str,
    entry: fn(),
    nice: i8,
) -> Result<Arc<Thread>> {
    let tid = alloc_tid();
    let vmar = proc.vmspace();
    let proc_weak = Arc::downgrade(proc);

    let name_buf = super::comm_from_name(name);

    let thread = Arc::new_cyclic(|weak_thread: &Weak<Thread>| {
        let weak_td_clone = weak_thread.clone();
        let options = TaskOptions::new(move || {
            entry();
        })
        .data(weak_td_clone)
        .local_data(vmar);

        let task = super::thread::build_task(options, nice);

        Thread {
            tid,
            td_proc: proc_weak,
            td_kstack: task,
            inner: SpinLock::new(ThreadInner {
                state: ThreadState::CanRun,
                flags: ThreadFlags::TDF_KTHREAD,
                sigmask: super::signal::SigSet::empty(),
                sigqueue: super::signal::SigQueue::new(),
                wchan: None,
                wmesg: "",
                name: name_buf,
                user_ctx: None,
            }),
        }
    });

    proc.add_thread(Arc::clone(&thread));
    thread.run();

    Ok(thread)
}
