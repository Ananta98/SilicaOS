// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    api::errno::{Errno, Result},
    proc::{fork::fork1, thread::Thread},
};

pub fn sys_fork(ctx: &mut UserContext) -> Result<usize> {
    let td = Thread::current().ok_or(Errno::ESRCH)?;
    let child_pid = fork1(&td, Some(ctx))?;
    Ok(child_pid.as_u32() as usize)
}
