// SPDX-License-Identifier: GPL-2.0

use ostd::arch::cpu::context::UserContext;
use crate::{
    errno::{Errno, Result},
    proc::thread::Thread,
    vm::{flags::MmapFlags, perms::VmPerms},
};

pub fn sys_mmap(args: &[usize; 6], _ctx: &mut UserContext) -> Result<usize> {
    let addr = args[0];
    let len = args[1];
    let prot_val = args[2] as u32;
    let flags_val = args[3] as u32;
    let _fd = args[4] as i32;
    let _offset = args[5];

    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let vmar = proc.vmspace();

    let mut perms = VmPerms::empty();
    if (prot_val & 1) != 0 { perms |= VmPerms::READ; }
    if (prot_val & 2) != 0 { perms |= VmPerms::WRITE; }
    if (prot_val & 4) != 0 { perms |= VmPerms::EXEC; }

    let mut flags = MmapFlags::empty();
    // Simplified mapping based on expected Linux constants for mmap
    // Linux MAP_SHARED = 0x01
    // Linux MAP_PRIVATE = 0x02
    // Linux MAP_FIXED = 0x10
    // Linux MAP_ANONYMOUS = 0x20
    if (flags_val & 0x02) != 0 { flags |= MmapFlags::PRIVATE; }
    if (flags_val & 0x01) != 0 { flags |= MmapFlags::SHARED; }
    if (flags_val & 0x10) != 0 { flags |= MmapFlags::FIXED; }
    if (flags_val & 0x20) != 0 { flags |= MmapFlags::ANONYMOUS; }

    let res = vmar.mmap_anonymous(addr, len, perms, flags)?;
    Ok(res)
}
