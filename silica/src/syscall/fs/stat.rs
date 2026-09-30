// SPDX-License-Identifier: GPL-2.0

//! The `stat` system call (Syscall #4 on x86_64).

use alloc::{string::String, vec::Vec};
use ostd::{
    arch::cpu::context::UserContext,
    mm::{io::FallibleVmRead, FallibleVmWrite},
};

use crate::{
    errno::{Errno, Result},
    fs::{lookup, root, vfs::LookupFlags},
    proc::thread::Thread,
    vm::vmar::Vmar,
};

/// Linux x86_64 ABI compatible `struct stat` (144 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: u64,
    pub st_atime_nsec: u64,
    pub st_mtime: u64,
    pub st_mtime_nsec: u64,
    pub st_ctime: u64,
    pub st_ctime_nsec: u64,
    pub __glibc_reserved: [i64; 3],
}

impl Stat {
    /// Serializes the stat structure into raw byte representation safely without unsafe.
    pub fn to_bytes(&self) -> [u8; 144] {
        let mut buf = [0u8; 144];
        buf[0..8].copy_from_slice(&self.st_dev.to_ne_bytes());
        buf[8..16].copy_from_slice(&self.st_ino.to_ne_bytes());
        buf[16..24].copy_from_slice(&self.st_nlink.to_ne_bytes());
        buf[24..28].copy_from_slice(&self.st_mode.to_ne_bytes());
        buf[28..32].copy_from_slice(&self.st_uid.to_ne_bytes());
        buf[32..36].copy_from_slice(&self.st_gid.to_ne_bytes());
        buf[36..40].copy_from_slice(&self.__pad0.to_ne_bytes());
        buf[40..48].copy_from_slice(&self.st_rdev.to_ne_bytes());
        buf[48..56].copy_from_slice(&self.st_size.to_ne_bytes());
        buf[56..64].copy_from_slice(&self.st_blksize.to_ne_bytes());
        buf[64..72].copy_from_slice(&self.st_blocks.to_ne_bytes());
        buf[72..80].copy_from_slice(&self.st_atime.to_ne_bytes());
        buf[80..88].copy_from_slice(&self.st_atime_nsec.to_ne_bytes());
        buf[88..96].copy_from_slice(&self.st_mtime.to_ne_bytes());
        buf[96..104].copy_from_slice(&self.st_mtime_nsec.to_ne_bytes());
        buf[104..112].copy_from_slice(&self.st_ctime.to_ne_bytes());
        buf[112..120].copy_from_slice(&self.st_ctime_nsec.to_ne_bytes());
        buf[120..128].copy_from_slice(&self.__glibc_reserved[0].to_ne_bytes());
        buf[128..136].copy_from_slice(&self.__glibc_reserved[1].to_ne_bytes());
        buf[136..144].copy_from_slice(&self.__glibc_reserved[2].to_ne_bytes());
        buf
    }
}

/// Safely copies a null-terminated string from userspace memory.
fn read_user_string(vmar: &Vmar, mut user_ptr: usize, max_len: usize) -> Result<String> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 64];

    while bytes.len() < max_len {
        let chunk_size = 64.min(max_len - bytes.len());
        let mut reader = vmar
            .vm_space()
            .reader(user_ptr, chunk_size)
            .map_err(|_| Errno::EFAULT)?;
        let mut writer = ostd::mm::VmWriter::from(&mut buf[..chunk_size]);
        reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;

        for i in 0..chunk_size {
            if buf[i] == 0 {
                let s = core::str::from_utf8(&bytes).map_err(|_| Errno::EINVAL)?;
                return Ok(String::from(s));
            }
            bytes.push(buf[i]);
        }
        user_ptr = user_ptr.checked_add(chunk_size).ok_or(Errno::EFAULT)?;
    }

    Err(Errno::ENAMETOOLONG)
}

/// System call `stat(const char *pathname, struct stat *statbuf)` (Syscall #4).
pub fn sys_stat(path_ptr: usize, statbuf_ptr: usize, _ctx: &mut UserContext) -> Result<usize> {
    let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
    let vmar = proc.vmspace();

    // Read pathname string from userspace
    let path = read_user_string(&vmar, path_ptr, 4096)?;

    // Lookup path in VFS
    let root_node = root()?;
    let path_node = lookup(root_node.clone(), root_node, &path, LookupFlags::empty())?;
    let inode = path_node.dentry.get_inode().ok_or(Errno::ENOENT)?;

    // Retrieve file attributes
    let attr = inode.node_ops.getattr().unwrap_or_else(|_| *inode.attr.read());

    let stat = Stat {
        st_dev: 1,
        st_ino: inode.id as u64,
        st_nlink: attr.nlink as u64,
        st_mode: attr.mode.bits(),
        st_uid: attr.uid,
        st_gid: attr.gid,
        __pad0: 0,
        st_rdev: attr.rdev,
        st_size: attr.size as i64,
        st_blksize: 4096,
        st_blocks: ((attr.size + 511) / 512) as i64,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
        __glibc_reserved: [0; 3],
    };

    // Copy stat buffer back to userspace
    let stat_bytes = stat.to_bytes();
    let mut writer = vmar
        .vm_space()
        .writer(statbuf_ptr, stat_bytes.len())
        .map_err(|_| Errno::EFAULT)?;
    let mut reader = ostd::mm::VmReader::from(&stat_bytes[..]);
    writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;

    Ok(0)
}
