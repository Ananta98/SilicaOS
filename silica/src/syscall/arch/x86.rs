// SPDX-License-Identifier: GPL-2.0

//! System call dispatch for x86_64 architecture matching Linux syscall ABI numbers.

use crate::syscall::{fs, mm, proc, sched};

crate::impl_syscall_nums_and_dispatch_fn! {
    SYS_READ = 0 => fs::sys_read(i32, usize, usize);
    SYS_WRITE = 1 => fs::sys_write(i32, usize, usize);
    SYS_STAT = 4 => fs::sys_stat(usize, usize);
    SYS_MMAP = 9 => mm::sys_mmap(usize, usize, u32, u32, i32, usize);
    SYS_MUNMAP = 11 => mm::sys_munmap(usize, usize);
    SYS_IOCTL = 16 => fs::sys_ioctl(i32, u32, usize);
    SYS_RT_SIGACTION = 13 => proc::sys_rt_sigaction(i32, usize, usize, usize);
    SYS_RT_SIGPROCMASK = 14 => proc::sys_rt_sigprocmask(i32, usize, usize, usize);
    SYS_SCHED_YIELD = 24 => sched::sys_sched_yield();
    SYS_GETPID = 39 => proc::sys_getpid();
    SYS_FORK = 57 => proc::sys_fork();
    SYS_KILL = 62 => proc::sys_kill(i32, i32);
    SYS_EXIT = 60 => proc::sys_exit(i32);
    SYS_WAIT4 = 61 => proc::sys_wait4(i32, usize, u32, usize);
    SYS_GETPPID = 110 => proc::sys_getppid();
    SYS_ARCH_PRCTL = 158 => proc::sys_arch_prctl(i32, usize);
    SYS_SIGALTSTACK = 131 => proc::sys_sigaltstack(usize, usize);
    SYS_SETPGID = 109 => proc::sys_setpgid(i32, i32);
    SYS_GETPGRP = 111 => proc::sys_getpgrp();
    SYS_SETSID = 112 => proc::sys_setsid();
    SYS_RT_SIGRETURN = 15 => proc::sys_rt_sigreturn();
    SYS_TGKILL = 234 => proc::sys_tgkill(i32, i32, i32);
}
