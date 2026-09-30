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

use ostd::user::UserContextApi;

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
pub mod proc;
pub mod syscall;

#[ostd::main]
fn kernel_main() {
    arch::init();
    vm::init();
    fs::init();
    sched::init();

    // Check for userspace init binary in initramfs
    let init_path = cmdline::get_init_path();
    let elf_data = fs::read_file_from_initramfs(&init_path)
        .or_else(|| fs::read_file_from_initramfs("/init"))
        .or_else(|| fs::read_file_from_initramfs("init"));

    if let Some(elf_data) = elf_data {
        ostd::info!("Booting userspace init from initramfs ({} bytes)", elf_data.len());
        match proc::exec::load_and_setup(elf_data, &[&init_path], &[]) {
            Ok((vmar, entry_point, sp)) => {
                let proc = proc::create_init_process_with_vmar(vmar)
                    .expect("failed to create init process");
                let mut user_ctx = ostd::arch::cpu::context::UserContext::default();
                user_ctx.set_instruction_pointer(entry_point);
                user_ctx.set_stack_pointer(sp);
                let thread = proc::create_main_thread(&proc, user_ctx)
                    .expect("failed to create init thread");
                thread.run();
                ostd::info!("Init process (PID 1) scheduled successfully at entry {:#x}", entry_point);
            }
            Err(err) => {
                ostd::error!("Failed to load userspace init ELF: {:?}", err);
            }
        }
    } else {
        ostd::warn!("No userspace init binary found in initramfs");
    }
}