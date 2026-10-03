// SPDX-License-Identifier: GPL-2.0

//! UNIX file permissions and access-control logic.
//!
//! Provides permission checking against process credentials (`Ucred`),
//! umask handling, sticky bit semantics, and standard mode bit definitions.

use alloc::sync::Arc;
use bitflags::bitflags;

use super::inode::{INodeAttr, Mode};
use crate::{
    api::cred::{Gid, Ucred, Uid},
    api::errno::{Errno, Result},
    proc::thread::Thread,
};

// Standard UNIX permission bitmasks
pub const S_ISUID: u32 = 0o4000; // Set user ID on execution
pub const S_ISGID: u32 = 0o2000; // Set group ID on execution
pub const S_ISVTX: u32 = 0o1000; // Sticky bit

pub const S_IRWXU: u32 = 0o700; // User read, write, execute
pub const S_IRUSR: u32 = 0o400; // User read
pub const S_IWUSR: u32 = 0o200; // User write
pub const S_IXUSR: u32 = 0o100; // User execute

pub const S_IRWXG: u32 = 0o070; // Group read, write, execute
pub const S_IRGRP: u32 = 0o040; // Group read
pub const S_IWGRP: u32 = 0o020; // Group write
pub const S_IXGRP: u32 = 0o010; // Group execute

pub const S_IRWXO: u32 = 0o007; // Other read, write, execute
pub const S_IROTH: u32 = 0o004; // Other read
pub const S_IWOTH: u32 = 0o002; // Other write
pub const S_IXOTH: u32 = 0o001; // Other execute

bitflags! {
    /// Requested access rights for access check.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct AccessFlags: u32 {
        const READ  = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC  = 1 << 2;
    }
}

/// Returns the credentials of the calling process, or root when there is no
/// process context (early boot, kernel threads without a `Proc`).
pub fn current_cred() -> Arc<Ucred> {
    match Thread::current_proc() {
        Some(proc) => proc.cred(),
        None => Arc::new(Ucred::root()),
    }
}

/// Checks whether a process with `cred` has `access` permissions to an inode with `attr`.
///
/// Implements standard POSIX 3-tier access control:
/// 1. Root superuser (UID 0): Read/write always allowed. Execute allowed if any execute bit is set.
/// 2. Owner: If effective UID matches file owner UID, check owner bits.
/// 3. Group: If effective GID (or any supplementary group) matches file GID, check group bits.
/// 4. Other: Check other bits.
pub fn check_permission(attr: &INodeAttr, cred: &Ucred, access: AccessFlags) -> Result<()> {
    // 1. Superuser check
    if cred.cr_uid.is_root() {
        if access.contains(AccessFlags::EXEC) {
            let mode_raw = attr.mode.bits();
            let any_exec = (mode_raw & (S_IXUSR | S_IXGRP | S_IXOTH)) != 0;
            if !any_exec && attr.mode.contains(Mode::FILE) {
                return Err(Errno::EACCES);
            }
        }
        return Ok(());
    }

    let mode_raw = attr.mode.bits();

    // 2. Owner check
    if cred.cr_uid == Uid(attr.uid) {
        if access.contains(AccessFlags::READ) && (mode_raw & S_IRUSR) == 0 {
            return Err(Errno::EACCES);
        }
        if access.contains(AccessFlags::WRITE) && (mode_raw & S_IWUSR) == 0 {
            return Err(Errno::EACCES);
        }
        if access.contains(AccessFlags::EXEC) && (mode_raw & S_IXUSR) == 0 {
            return Err(Errno::EACCES);
        }
        return Ok(());
    }

    // 3. Group check
    if cred.cr_gid == Gid(attr.gid) || cred.cr_groups.contains(&Gid(attr.gid)) {
        if access.contains(AccessFlags::READ) && (mode_raw & S_IRGRP) == 0 {
            return Err(Errno::EACCES);
        }
        if access.contains(AccessFlags::WRITE) && (mode_raw & S_IWGRP) == 0 {
            return Err(Errno::EACCES);
        }
        if access.contains(AccessFlags::EXEC) && (mode_raw & S_IXGRP) == 0 {
            return Err(Errno::EACCES);
        }
        return Ok(());
    }

    // 4. Other check
    if access.contains(AccessFlags::READ) && (mode_raw & S_IROTH) == 0 {
        return Err(Errno::EACCES);
    }
    if access.contains(AccessFlags::WRITE) && (mode_raw & S_IWOTH) == 0 {
        return Err(Errno::EACCES);
    }
    if access.contains(AccessFlags::EXEC) && (mode_raw & S_IXOTH) == 0 {
        return Err(Errno::EACCES);
    }

    Ok(())
}

/// Applies creation umask (`cmask`) to requested mode flags.
pub fn apply_umask(mode: Mode, umask: u32) -> Mode {
    let raw = mode.bits();
    let perm_mask = 0o7777;
    let file_type = raw & !perm_mask;
    let perms = (raw & perm_mask) & !(umask & 0o777);
    Mode::from_bits_truncate(file_type | perms)
}

/// Checks sticky bit (`S_ISVTX`) restrictions for deletion/renaming in a directory.
///
/// In a sticky directory, an entry can only be unlinked or renamed by:
/// - The superuser.
/// - The owner of the directory.
/// - The owner of the file being removed.
pub fn check_sticky(dir_attr: &INodeAttr, target_attr: &INodeAttr, cred: &Ucred) -> Result<()> {
    let is_sticky = (dir_attr.mode.bits() & S_ISVTX) != 0;
    if !is_sticky || cred.cr_uid.is_root() {
        return Ok(());
    }

    if cred.cr_uid == Uid(dir_attr.uid) || cred.cr_uid == Uid(target_attr.uid) {
        return Ok(());
    }

    Err(Errno::EPERM)
}
