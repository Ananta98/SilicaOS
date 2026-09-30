// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::{thread::Thread, wait::{IdType, WaitOptions, kern_wait6}},
};

/// sys_wait4(pid, status, options, rusage)
pub fn sys_wait4(args: &[usize; 6], _ctx: &mut UserContext) -> Result<usize> {
    let pid_val = args[0] as i32;
    let status_ptr = args[1];
    let options_val = args[2] as u32;
    let _rusage_ptr = args[3];

    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    
    let idtype = if pid_val < -1 {
        IdType::Pgid((-pid_val) as u32)
    } else if pid_val == -1 {
        IdType::All
    } else if pid_val == 0 {
        IdType::Pgid(proc.pid.as_u32()) // wait for any child in same pgid
    } else {
        let pid = crate::proc::tree::Pid::from_u32(pid_val as u32).ok_or(Errno::EINVAL)?;
        IdType::Pid(pid)
    };

    let options = WaitOptions::from_bits_truncate(options_val);
    let res = kern_wait6(&proc, idtype, options)?;

    match res {
        Some(wait_res) => {
            if status_ptr != 0 {
                let vmar = proc.vmspace();
                let raw_status = wait_res.status.as_raw();
                let mut writer = vmar.vm_space().writer(status_ptr, core::mem::size_of::<i32>()).map_err(|_| Errno::EFAULT)?;
                writer.write_val(&raw_status).map_err(|_| Errno::EFAULT)?;
            }
            Ok(wait_res.pid.as_u32() as usize)
        }
        None => Ok(0),
    }
}
