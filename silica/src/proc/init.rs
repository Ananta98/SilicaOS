// SPDX-License-Identifier: GPL-2.0

//! Initial user process (`init`, PID 1) bootstrap and execution.
//!
//! Inspired by FreeBSD `sys/kern/init_main.c` and Asterinas `init_proc.rs`.
//! Locates the initial userspace binary, creates the root address space,
//! binds standard descriptors, and launches PID 1.

use alloc::{string::String, sync::Arc};
use ostd::arch::cpu::context::UserContext;
use ostd::user::UserContextApi;

use crate::{
    api::errno::{Errno, Result},
    cmdline, fs,
    proc::{Proc, create_init_process_with_vmar, create_main_thread, exec::load_and_setup},
};

/// Candidate binary paths inspected when looking for the initial userspace process.
const INIT_CANDIDATE_PATHS: &[&str] = &["/init", "init", "/sbin/init", "/bin/init", "/bin/sh"];

/// Attempts to locate the init ELF binary across candidate locations.
fn find_init_binary() -> Option<(String, &'static [u8])> {
    // 1. Try explicit command line parameter (e.g., "init=/bin/sh")
    let cmd_init = cmdline::get_init_path();
    if !cmd_init.is_empty() {
        if let Some(elf_data) = fs::read_file_from_initramfs(&cmd_init) {
            return Some((cmd_init, elf_data));
        }
    }

    // 2. Try standard candidate paths
    for &candidate in INIT_CANDIDATE_PATHS {
        if let Some(elf_data) = fs::read_file_from_initramfs(candidate) {
            return Some((String::from(candidate), elf_data));
        }
    }

    None
}

/// Spawns and schedules the initial user process (`init`, PID 1).
///
/// Returns the newly created process descriptor on success.
pub fn spawn_init_process() -> Result<Arc<Proc>> {
    let (init_path, elf_data) = match find_init_binary() {
        Some((path, data)) => (path, data),
        None => {
            ostd::warn!("init: No userspace init binary found in initramfs or root filesystem");
            return Err(Errno::ENOENT);
        }
    };

    ostd::info!(
        "init: Booting userspace init from \"{}\" ({} bytes)",
        init_path,
        elf_data.len()
    );

    // 1. Build address space, map ELF segments, setup AMD64 ABI user stack
    let (vmar, entry_point, sp) = load_and_setup(elf_data, &[&init_path], &[])?;

    // 2. Create Process Control Block for PID 1 with root credentials and stdio
    //
    // `Proc::new` already marks PID 1 as the reaper, so there is nothing to set
    // here.
    let proc = create_init_process_with_vmar(vmar)?;

    // 3. Set up user context
    let mut user_ctx = UserContext::default();
    user_ctx.set_instruction_pointer(entry_point);
    user_ctx.set_stack_pointer(sp);

    // 4. Create and run primary init thread
    let thread = create_main_thread(&proc, user_ctx)?;
    thread.run();

    ostd::info!(
        "init: Init process (PID 1) scheduled successfully (entry: {:#x}, sp: {:#x})",
        entry_point,
        sp
    );

    Ok(proc)
}
