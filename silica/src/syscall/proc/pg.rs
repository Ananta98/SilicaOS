// SPDX-License-Identifier: GPL-2.0

use crate::errno::Result;
use ostd::arch::cpu::context::UserContext;

pub fn sys_setpgid(_pid: i32, _pgid: i32, _ctx: &mut UserContext) -> Result<usize> {
    crate::return_errno!(ENOSYS, "setpgid not implemented")
}

pub fn sys_getpgrp(_ctx: &mut UserContext) -> Result<usize> {
    crate::return_errno!(ENOSYS, "getpgrp not implemented")
}

pub fn sys_setsid(_ctx: &mut UserContext) -> Result<usize> {
    crate::return_errno!(ENOSYS, "setsid not implemented")
}
