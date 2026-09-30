// SPDX-License-Identifier: GPL-2.0

use ostd::{arch::cpu::context::UserContext, task::Task};
use crate::errno::Result;

pub fn sys_sched_yield(_ctx: &mut UserContext) -> Result<usize> {
    Task::yield_now();
    Ok(0)
}
