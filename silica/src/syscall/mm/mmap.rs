// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::thread::Thread,
    vm::{flags::MmapFlags, perms::VmPerms},
};

pub fn sys_mmap(
    addr: usize,
    len: usize,
    prot_val: u32,
    flags_val: u32,
    _fd: i32,
    _offset: usize,
    _ctx: &mut UserContext
) -> Result<usize> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let vmar = proc.vmspace();

    let mut perms = VmPerms::empty();
    if (prot_val & 1) != 0 { perms |= VmPerms::READ; }
    if (prot_val & 2) != 0 { perms |= VmPerms::WRITE; }
    if (prot_val & 4) != 0 { perms |= VmPerms::EXEC; }

    let mut flags = MmapFlags::empty();
    if (flags_val & 0x02) != 0 { flags |= MmapFlags::PRIVATE; }
    if (flags_val & 0x01) != 0 { flags |= MmapFlags::SHARED; }
    if (flags_val & 0x10) != 0 { flags |= MmapFlags::FIXED; }
    if (flags_val & 0x20) != 0 { flags |= MmapFlags::ANONYMOUS; }

    let res = vmar.mmap_anonymous(addr, len, perms, flags)?;
    Ok(res)
}
