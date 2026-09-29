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

pub mod vm;
pub mod fs;
pub mod errno;
pub mod sched;
pub mod utils;
pub mod cmdline;

#[ostd::main]
fn kernel_main() {
    arch::init();
    vm::init();
    fs::init();
    sched::init();
    sched::selftest::run();
}

