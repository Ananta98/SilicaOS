// SPDX-License-Identifier: GPL-2.0

//! SilicaOS: a POSIX-oriented operating system kernel.
//!
//! The kernel is split into subsystems, each of which lives in its own top-level
//! module:
//!
//! - [`arch`]: architecture-specific bring-up.
//! - [`api`]: the types and constants user space sees — `errno` numbers, `uid`
//!   and `gid`, resource limits, signal dispositions, `termios`, `ioctl`
//!   requests. Layout-sensitive, so it changes with the ABI rather than with
//!   kernel logic.
//! - [`cmdline`]: the kernel command line.
//! - [`drivers`]: device drivers.
//! - [`fs`]: filesystems and the virtual file system.
//! - [`modules`]: loadable kernel modules and their initcalls.
//! - [`proc`]: processes, threads, and their lifecycle.
//! - [`sched`]: task scheduling.
//! - [`syscall`]: the system call table and dispatch.
//! - [`vm`]: virtual memory management, i.e. the mappings of an address space.
//!
//! [`arch`]: arch

#![no_std]
#![deny(unsafe_code)]
#![allow(dead_code)]

extern crate alloc;

macro_rules! __log_prefix {
    () => {
        ""
    };
}

#[cfg_attr(target_arch = "x86_64", path = "arch/x86_64/mod.rs")]
pub mod arch;

pub mod api;
pub mod cmdline;
pub mod drivers;
pub mod fs;
pub mod modules;
pub mod proc;
pub mod sched;
pub mod syscall;
pub mod utils;
pub mod vm;

#[ostd::main]
fn kernel_main() {
    cmdline::init(&ostd::boot::boot_info().kernel_cmdline);
    arch::init();
    vm::init();
    fs::init();
    sched::init();
    if let Err(err) = drivers::init() {
        ostd::error!("Drivers init failed: {:?}", err);
    }
    if let Err(err) = modules::init_calls() {
        ostd::error!("Module initcalls failed: {:?}", err);
    }
    vm::reclaim::init_kswapd();
    fs::auto_probe_and_mount();
    if let Err(err) = proc::init::spawn_init_process() {
        ostd::warn!("Init process could not be launched: {:?}", err);
    }
}
