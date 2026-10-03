// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    api::errno::{Errno, Result},
    proc::thread::Thread,
};

pub fn sys_getppid(_ctx: &mut UserContext) -> Result<usize> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let ppid = proc.inner.lock().ppid.map(|p| p.as_u32()).unwrap_or(0);
    Ok(ppid as usize)
}
