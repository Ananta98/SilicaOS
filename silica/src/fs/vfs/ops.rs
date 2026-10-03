// SPDX-License-Identifier: GPL-2.0

//! High-level namespace operations.
//!
//! These wrap the per-filesystem [`NodeOps`] with the policy every caller needs
//! and no driver should reimplement: parent resolution, permission and sticky
//! bit checks, umask, cross-mount rejection, and keeping the dentry cache
//! coherent with what the filesystem just did.

use alloc::string::String;
use alloc::sync::Arc;
use spin::Mutex;

use crate::api::errno::{Errno, Result};
use crate::proc::thread::Thread;
use super::dcache::DEntry;
use super::fs::StatFs;
use super::inode::{INode, INodeAttr, Mode, S_IFMT};
use super::mount::{root, PathNode};
use super::path::{lookup, LookupFlags};
use super::perms::{apply_umask, check_permission, check_sticky, current_cred, AccessFlags};

/// Longest single path component.
pub const NAME_MAX: usize = 255;

/// Serializes `rename` so the "not into its own subtree" check cannot race
/// with another rename rewriting the tree.
static RENAME_LOCK: Mutex<()> = Mutex::new(());

/// The calling process' file-creation mask (0 when there is no process).
fn current_umask() -> u32 {
    Thread::current_proc().map_or(0, |p| p.fd_table.lock().cmask)
}

/// Splits `path` into the directory part and the final component.
fn split_parent(path: &str) -> Result<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // "/" or "": there is no final component to operate on.
        return Err(if path.is_empty() { Errno::ENOENT } else { Errno::EBUSY });
    }
    let (dir, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => ("", trimmed),
    };
    if name == "." || name == ".." {
        return Err(Errno::EINVAL);
    }
    if name.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok((dir, name))
}

/// Resolves the parent directory of `path`, checking it is writable and
/// searchable by the caller. Returns the directory, its inode, and the name.
fn parent_for_modify(path: &str) -> Result<(PathNode, Arc<INode>, String)> {
    let (dir, name) = split_parent(path)?;
    let r = root()?;
    let parent = lookup(r.clone(), r, dir, LookupFlags::DIRECTORY)?;
    let inode = parent.dentry.get_inode().ok_or(Errno::ENOENT)?;
    let attr = *inode.attr.read();
    check_permission(&attr, &current_cred(), AccessFlags::WRITE | AccessFlags::EXEC)?;
    Ok((parent, inode, name.into()))
}

/// Looks up `path` without following a final symlink.
fn lookup_nofollow(path: &str) -> Result<PathNode> {
    let r = root()?;
    lookup(r.clone(), r, path, LookupFlags::NO_FOLLOW)
}

/// Whether `name` already exists in `parent` (checked through the filesystem,
/// so a stale dcache cannot hide an entry).
fn exists_in(parent: &PathNode, inode: &Arc<INode>, name: &str) -> Result<bool> {
    if parent.dentry.lookup_child(name).is_some_and(|d| d.get_inode().is_some()) {
        return Ok(true);
    }
    match inode.node_ops.lookup(name) {
        Ok(_) => Ok(true),
        Err(Errno::ENOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Publishes a freshly created inode into the dcache and stamps ownership.
fn publish(parent: &PathNode, name: &str, node: &Arc<INode>) {
    let cred = current_cred();
    {
        let mut attr = node.attr.write();
        attr.uid = cred.cr_uid.as_u32();
        attr.gid = cred.cr_gid.as_u32();
    }
    let dentry = Arc::new(DEntry::new(
        name.into(),
        Some(node.clone()),
        Some(Arc::downgrade(&parent.dentry)),
    ));
    parent.dentry.add_child(dentry);
}

fn with_type(ty: Mode, perm: Mode) -> Mode {
    let masked = apply_umask(Mode::from_bits_truncate(perm.bits() & 0o7777), current_umask());
    Mode::from_bits_truncate(ty.bits() | masked.bits())
}

/// Creates a regular file.
pub fn create(path: &str, mode: Mode) -> Result<Arc<INode>> {
    let (parent, dir, name) = parent_for_modify(path)?;
    if exists_in(&parent, &dir, &name)? {
        return Err(Errno::EEXIST);
    }
    let node = dir.node_ops.create(&name, with_type(Mode::FILE, mode))?;
    publish(&parent, &name, &node);
    dir.touch_mtime();
    Ok(node)
}

/// Creates a directory.
pub fn mkdir(path: &str, mode: Mode) -> Result<Arc<INode>> {
    let (parent, dir, name) = parent_for_modify(path)?;
    if exists_in(&parent, &dir, &name)? {
        return Err(Errno::EEXIST);
    }
    let node = dir.node_ops.mkdir(&name, with_type(Mode::DIR, mode))?;
    publish(&parent, &name, &node);
    dir.touch_mtime();
    Ok(node)
}

/// Creates a special file. `mode` carries the file-type bits.
///
/// Device nodes require superuser, as in POSIX.
pub fn mknod(path: &str, mode: Mode, rdev: u64) -> Result<Arc<INode>> {
    let ty = mode.bits() & S_IFMT;
    let is_device = ty == Mode::CHAR.bits() || ty == Mode::BLOCK.bits();
    let allowed = matches!(
        ty,
        t if t == Mode::FILE.bits()
            || t == Mode::FIFO.bits()
            || t == Mode::SOCKET.bits()
            || is_device
    );
    if !allowed {
        return Err(Errno::EINVAL);
    }
    if is_device && !current_cred().is_superuser() {
        return Err(Errno::EPERM);
    }
    let (parent, dir, name) = parent_for_modify(path)?;
    if exists_in(&parent, &dir, &name)? {
        return Err(Errno::EEXIST);
    }
    let node = dir
        .node_ops
        .mknod(&name, with_type(Mode::from_bits_truncate(ty), mode), rdev)?;
    publish(&parent, &name, &node);
    dir.touch_mtime();
    Ok(node)
}

/// Creates a symbolic link at `linkpath` whose contents are `target`.
pub fn symlink(target: &str, linkpath: &str) -> Result<Arc<INode>> {
    if target.is_empty() {
        return Err(Errno::ENOENT);
    }
    let (parent, dir, name) = parent_for_modify(linkpath)?;
    if exists_in(&parent, &dir, &name)? {
        return Err(Errno::EEXIST);
    }
    let node = dir.node_ops.symlink(&name, target)?;
    publish(&parent, &name, &node);
    dir.touch_mtime();
    Ok(node)
}

/// Reads the target of the symlink at `path`.
pub fn readlink(path: &str) -> Result<String> {
    let node = lookup_nofollow(path)?;
    let inode = node.dentry.get_inode().ok_or(Errno::ENOENT)?;
    if !inode.is_symlink() {
        return Err(Errno::EINVAL);
    }
    inode.touch_atime();
    inode.node_ops.readlink()
}

/// Removes a non-directory entry.
pub fn unlink(path: &str) -> Result<()> {
    let (parent, dir, name) = parent_for_modify(path)?;
    let target = lookup_nofollow(path)?;
    let inode = target.dentry.get_inode().ok_or(Errno::ENOENT)?;
    if inode.is_dir() {
        return Err(Errno::EISDIR);
    }
    let dir_attr = *dir.attr.read();
    check_sticky(&dir_attr, &*inode.attr.read(), &current_cred())?;

    dir.node_ops.unlink(&name)?;
    parent.dentry.remove_child(&name);
    {
        let mut a = inode.attr.write();
        a.nlink = a.nlink.saturating_sub(1);
    }
    inode.touch_ctime();
    dir.touch_mtime();
    Ok(())
}

/// Removes an empty directory.
pub fn rmdir(path: &str) -> Result<()> {
    let (parent, dir, name) = parent_for_modify(path)?;
    // Something mounted here makes the directory busy, not removable.
    if parent
        .dentry
        .lookup_child(&name)
        .is_some_and(|d| !d.mounts.read().is_empty())
    {
        return Err(Errno::EBUSY);
    }
    let target = lookup_nofollow(path)?;
    let inode = target.dentry.get_inode().ok_or(Errno::ENOENT)?;
    if !inode.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    let dir_attr = *dir.attr.read();
    check_sticky(&dir_attr, &*inode.attr.read(), &current_cred())?;

    dir.node_ops.rmdir(&name)?;
    parent.dentry.remove_child(&name);
    dir.touch_mtime();
    Ok(())
}

/// Creates a hard link `new` to the existing non-directory `old`.
pub fn link(old: &str, new: &str) -> Result<()> {
    let old_node = lookup_nofollow(old)?;
    let inode = old_node.dentry.get_inode().ok_or(Errno::ENOENT)?;
    if inode.is_dir() {
        return Err(Errno::EPERM);
    }
    let (parent, dir, name) = parent_for_modify(new)?;
    if !Arc::ptr_eq(&parent.mount, &old_node.mount) {
        return Err(Errno::EXDEV);
    }
    if exists_in(&parent, &dir, &name)? {
        return Err(Errno::EEXIST);
    }
    dir.node_ops.link(&name, inode.clone())?;
    parent.dentry.add_child(Arc::new(DEntry::new(
        name,
        Some(inode.clone()),
        Some(Arc::downgrade(&parent.dentry)),
    )));
    inode.attr.write().nlink += 1;
    inode.touch_ctime();
    dir.touch_mtime();
    Ok(())
}

/// Renames `old` to `new`, replacing `new` if it is compatible.
pub fn rename(old: &str, new: &str) -> Result<()> {
    let _guard = RENAME_LOCK.lock();

    let (old_parent, old_dir, old_name) = parent_for_modify(old)?;
    let (new_parent, new_dir, new_name) = parent_for_modify(new)?;
    if !Arc::ptr_eq(&old_parent.mount, &new_parent.mount) {
        return Err(Errno::EXDEV);
    }

    let src = lookup_nofollow(old)?;
    let src_inode = src.dentry.get_inode().ok_or(Errno::ENOENT)?;
    let cred = current_cred();
    check_sticky(&*old_dir.attr.read(), &*src_inode.attr.read(), &cred)?;

    // Existing destination: must be type-compatible and the same-inode case is a no-op.
    match lookup_nofollow(new) {
        Ok(dst) => {
            let dst_inode = dst.dentry.get_inode().ok_or(Errno::ENOENT)?;
            if Arc::ptr_eq(&src_inode, &dst_inode) {
                return Ok(());
            }
            match (src_inode.is_dir(), dst_inode.is_dir()) {
                (true, false) => return Err(Errno::ENOTDIR),
                (false, true) => return Err(Errno::EISDIR),
                _ => {}
            }
            if !dst.dentry.mounts.read().is_empty() {
                return Err(Errno::EBUSY);
            }
            check_sticky(&*new_dir.attr.read(), &*dst_inode.attr.read(), &cred)?;
        }
        Err(Errno::ENOENT) => {}
        Err(e) => return Err(e),
    }

    // A directory must not move into its own subtree.
    if src_inode.is_dir() {
        let mut cur = Some(new_parent.dentry.clone());
        while let Some(d) = cur {
            if Arc::ptr_eq(&d, &src.dentry) {
                return Err(Errno::EINVAL);
            }
            cur = d.parent.as_ref().and_then(|w| w.upgrade());
        }
    }

    old_dir.node_ops.rename(&old_name, &new_dir, &new_name)?;

    // Dentries are immutable in name/parent, so re-home the entry.
    old_parent.dentry.remove_child(&old_name);
    new_parent.dentry.remove_child(&new_name);
    new_parent.dentry.add_child(Arc::new(DEntry::new(
        new_name,
        Some(src_inode.clone()),
        Some(Arc::downgrade(&new_parent.dentry)),
    )));

    src_inode.touch_ctime();
    old_dir.touch_mtime();
    new_dir.touch_mtime();
    Ok(())
}

/// Sets the size of the regular file at `path`.
pub fn truncate(path: &str, size: usize) -> Result<()> {
    let r = root()?;
    let node = lookup(r.clone(), r, path, LookupFlags::empty())?;
    let inode = node.dentry.get_inode().ok_or(Errno::ENOENT)?;
    if inode.is_dir() {
        return Err(Errno::EISDIR);
    }
    if !inode.is_file() {
        return Err(Errno::EINVAL);
    }
    let attr: INodeAttr = *inode.attr.read();
    check_permission(&attr, &current_cred(), AccessFlags::WRITE)?;
    let mut new_attr = attr;
    new_attr.size = size;
    inode.node_ops.setattr(&new_attr)?;
    inode.set_size(size);
    inode.touch_mtime();
    Ok(())
}

/// Unmounts the filesystem mounted at `path`. Superuser only.
pub fn umount(path: &str) -> Result<()> {
    if !current_cred().is_superuser() {
        return Err(Errno::EPERM);
    }
    let r = root()?;
    let node = lookup(r.clone(), r, path, LookupFlags::DIRECTORY)?;
    node.umount()
}

/// Reports statistics of the filesystem containing `path`.
pub fn statfs(path: &str) -> Result<StatFs> {
    let r = root()?;
    let node = lookup(r.clone(), r, path, LookupFlags::empty())?;
    node.mount.fs.statfs()
}
