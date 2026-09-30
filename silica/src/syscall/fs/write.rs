// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::thread::Thread,
};
use ostd::mm::io::{FallibleVmRead, FallibleVmWrite};

pub fn sys_write(args: &[usize; 6], _ctx: &mut UserContext) -> Result<usize> {
    let fd = args[0] as i32;
    let buf_ptr = args[1];
    let count = args[2];

    if count == 0 {
        return Ok(0);
    }
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let vmar = proc.vmspace();

    let mut kbuf = alloc::vec![0u8; count.min(65536)];
    let copy_len = kbuf.len();

    let mut reader = vmar.vm_space().reader(buf_ptr, copy_len).map_err(|_| Errno::EFAULT)?;
    let mut writer = ostd::mm::VmWriter::from(&mut kbuf[..]);
    reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;

    if fd == 1 || fd == 2 {
        if let Ok(s) = core::str::from_utf8(&kbuf[..copy_len]) {
            ostd::info!("[USER]: {}", s.trim_end_matches('\n'));
        }
        return Ok(copy_len);
    }

    let file = proc.fd_table.lock().get(fd)?;
    file.write(&kbuf[..copy_len])
}
