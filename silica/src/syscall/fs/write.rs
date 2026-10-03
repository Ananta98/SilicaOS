// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    api::errno::{Errno, Result},
    proc::thread::Thread,
};
use ostd::mm::io::FallibleVmRead;

pub fn sys_write(fd: i32, buf_ptr: usize, count: usize, _ctx: &mut UserContext) -> Result<usize> {
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

    let file = proc.fd_table.lock().get(fd)?;
    file.write(&kbuf[..copy_len])
}
