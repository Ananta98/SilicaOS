// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    api::errno::Result,
    proc::{ExitStatus, exit::exit1, thread::Thread},
};

pub fn sys_exit(code: i32, _ctx: &mut UserContext) -> Result<usize> {
    let td = Thread::current().expect("must be in thread context");
    exit1(&td, ExitStatus::Exited(code));
    Ok(0)
}
