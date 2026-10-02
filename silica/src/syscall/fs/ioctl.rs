// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::thread::Thread,
};

pub fn sys_ioctl(fd: i32, cmd: u32, arg: usize, _ctx: &mut UserContext) -> Result<usize> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let file = proc.fd_table.lock().get(fd)?;
    file.ioctl(cmd, arg)
}
