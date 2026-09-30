// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::{ExitStatus, exit::exit1, thread::Thread},
};

pub fn sys_exit(args: &[usize; 6], _ctx: &mut UserContext) -> Result<usize> {
    let code = args[0] as i32;
    let td = Thread::current().expect("must be in thread context");
    exit1(&td, ExitStatus::Exited(code));
    Ok(0)
}
