// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::thread::Thread,
};

pub fn sys_read(fd: i32, buf_ptr: usize, count: usize, _ctx: &mut UserContext) -> Result<usize> {
    if count == 0 {
        return Ok(0);
    }
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let file = proc.fd_table.lock().get(fd)?;

    let mut kbuf = alloc::vec![0u8; count.min(65536)];
    let nread = file.read(&mut kbuf)?;

    if nread > 0 {
        let vmar = proc.vmspace();
        let mut writer = vmar.vm_space().writer(buf_ptr, nread).map_err(|_| Errno::EFAULT)?;
        let mut reader = ostd::mm::VmReader::from(&kbuf[..nread]);
        writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
    }

    Ok(nread)
}
