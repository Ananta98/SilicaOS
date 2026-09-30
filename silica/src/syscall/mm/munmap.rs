// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::thread::Thread,
};

pub fn sys_munmap(args: &[usize; 6], _ctx: &mut UserContext) -> Result<usize> {
    let addr = args[0];
    let len = args[1];

    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    proc.vmspace().munmap(addr..addr + len)?;
    Ok(0)
}
