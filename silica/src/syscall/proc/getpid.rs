// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    api::errno::{Errno, Result},
    proc::thread::Thread,
};

pub fn sys_getpid(_ctx: &mut UserContext) -> Result<usize> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    Ok(proc.pid.as_u32() as usize)
}
