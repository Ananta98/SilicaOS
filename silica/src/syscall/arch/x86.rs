// SPDX-License-Identifier: GPL-2.0

//! System call dispatch for x86_64 architecture.

use crate::syscall::{fs, proc, mm, sched};

crate::impl_syscall_nums_and_dispatch_fn! {
    SYS_READ = 0 => fs::sys_read;
    SYS_WRITE = 1 => fs::sys_write;
    SYS_MMAP = 9 => mm::sys_mmap;
    SYS_MUNMAP = 11 => mm::sys_munmap;
    SYS_SCHED_YIELD = 24 => sched::sys_sched_yield;
    SYS_GETPID = 39 => proc::sys_getpid;
    SYS_FORK = 57 => proc::sys_fork;
    SYS_EXIT = 60 => proc::sys_exit;
    SYS_WAIT4 = 61 => proc::sys_wait4;
    SYS_GETPPID = 110 => proc::sys_getppid;
}
