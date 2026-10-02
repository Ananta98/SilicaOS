// SPDX-License-Identifier: GPL-2.0

//! SilicaOS: a POSIX-oriented operating system kernel.
//!
//! The kernel is split into subsystems, each of which lives in its own top-level
//! module:
//!
//! - [`arch`]: architecture-specific bring-up.
//! - [`errno`]: the POSIX error numbers that subsystems report.
//! - [`vm`]: virtual memory management, i.e. the mappings of an address space.
//! - [`sched`]: task scheduling.
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
pub mod errno;
pub mod fs;
pub mod modules;
pub mod proc;
pub mod sched;
pub mod syscall;
pub mod utils;
pub mod vm;

#[ostd::main]
fn kernel_main() {
    arch::init();
    vm::init();
    fs::init();
    sched::init();

    if let Err(err) = drivers::init() {
        ostd::error!("Drivers init failed: {:?}", err);
    }

    // Initialize all kernel module drivers declared via `module!` macros
    if let Err(err) = modules::init_calls() {
        ostd::error!("Module initcalls failed: {:?}", err);
    }

    // Start background memory reclaimer daemon
    vm::reclaim::init_kswapd();

    // Spawn initial userspace process (PID 1)
    if let Err(err) = proc::init::spawn_init_process() {
        ostd::warn!("Init process could not be launched: {:?}", err);
    }
}
