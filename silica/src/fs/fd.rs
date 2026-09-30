// SPDX-License-Identifier: GPL-2.0

//! File descriptor table (FreeBSD `sys/filedesc.h`).
//!
//! Manages open file descriptors, `FD_CLOEXEC` flags, descriptor duplication,
//! and close-on-exec lifecycle actions.

use alloc::{sync::Arc, vec::Vec};
use bitflags::bitflags;

use crate::errno::Result;
use crate::fs::devfs::{ConsoleFile, NullFile};
use crate::fs::vfs::File;

bitflags! {
    /// File descriptor flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct FdFlags: u32 {
        /// Close the descriptor during `execve(2)`.
        const FD_CLOEXEC = 1 << 0;
    }
}

/// An entry in the file descriptor table.
#[derive(Clone)]
pub struct FdEntry {
    pub file: Arc<File>,
    pub flags: FdFlags,
}

/// Process file descriptor table mirroring FreeBSD `struct filedesc`.
pub struct Filedesc {
    pub files: Vec<Option<FdEntry>>,
    pub cmask: u32,
    pub max_files: usize,
}

impl Filedesc {
    /// Default maximum open files per process.
    pub const DEFAULT_MAX_FILES: usize = 1024;

    /// Creates an empty file descriptor table.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            cmask: 0o022,
            max_files: Self::DEFAULT_MAX_FILES,
        }
    }

    /// Creates a table with stdin (0) as NullFile, stdout (1) and stderr (2) as ConsoleFile.
    pub fn with_stdio() -> Self {
        let mut table = Self::new();
        let null = NullFile::new_file();
        let console1 = ConsoleFile::new_file();
        let console2 = ConsoleFile::new_file();
        let _ = table.alloc_fd(null, FdFlags::empty());
        let _ = table.alloc_fd(console1, FdFlags::empty());
        let _ = table.alloc_fd(console2, FdFlags::empty());
        table
    }

    /// Allocates the lowest available file descriptor for `file`.
    pub fn alloc_fd(&mut self, file: Arc<File>, flags: FdFlags) -> Result<i32> {
        self.alloc_fd_at(0, file, flags)
    }

    /// Allocates the lowest available file descriptor greater than or equal to `min_fd`.
    pub fn alloc_fd_at(&mut self, min_fd: i32, file: Arc<File>, flags: FdFlags) -> Result<i32> {
        if min_fd < 0 || min_fd as usize >= self.max_files {
            crate::return_errno!(EINVAL, "invalid minimum file descriptor {min_fd}");
        }
        let min_idx = min_fd as usize;
        if self.files.len() <= min_idx {
            self.files.resize(min_idx, None);
        }
        for (idx, slot) in self.files.iter_mut().enumerate().skip(min_idx) {
            if slot.is_none() {
                *slot = Some(FdEntry { file, flags });
                return Ok(idx as i32);
            }
        }
        if self.files.len() >= self.max_files {
            crate::return_errno!(EMFILE, "too many open files");
        }
        let new_fd = self.files.len() as i32;
        self.files.push(Some(FdEntry { file, flags }));
        Ok(new_fd)
    }

    /// Retrieves the file referenced by `fd`.
    pub fn get(&self, fd: i32) -> Result<Arc<File>> {
        if fd < 0 {
            crate::return_errno!(EBADF, "negative file descriptor {fd}");
        }
        let idx = fd as usize;
        match self.files.get(idx) {
            Some(Some(entry)) => Ok(Arc::clone(&entry.file)),
            _ => crate::return_errno!(EBADF, "file descriptor {fd} is not open"),
        }
    }

    /// Retrieves descriptor flags for `fd`.
    pub fn get_flags(&self, fd: i32) -> Result<FdFlags> {
        if fd < 0 {
            crate::return_errno!(EBADF, "negative file descriptor {fd}");
        }
        let idx = fd as usize;
        match self.files.get(idx) {
            Some(Some(entry)) => Ok(entry.flags),
            _ => crate::return_errno!(EBADF, "file descriptor {fd} is not open"),
        }
    }

    /// Sets descriptor flags for `fd`.
    pub fn set_flags(&mut self, fd: i32, flags: FdFlags) -> Result<()> {
        if fd < 0 {
            crate::return_errno!(EBADF, "negative file descriptor {fd}");
        }
        let idx = fd as usize;
        match self.files.get_mut(idx) {
            Some(Some(entry)) => {
                entry.flags = flags;
                Ok(())
            }
            _ => crate::return_errno!(EBADF, "file descriptor {fd} is not open"),
        }
    }

    /// Closes a file descriptor.
    pub fn close(&mut self, fd: i32) -> Result<()> {
        if fd < 0 {
            crate::return_errno!(EBADF, "negative file descriptor {fd}");
        }
        let idx = fd as usize;
        if idx >= self.files.len() || self.files[idx].is_none() {
            crate::return_errno!(EBADF, "file descriptor {fd} is not open");
        }
        let entry = self.files[idx].take();
        if let Some(entry) = entry {
            let _ = entry.file.close();
        }
        Ok(())
    }

    /// Duplicates `oldfd` onto `newfd` (`dup2(2)`).
    pub fn dup2(&mut self, oldfd: i32, newfd: i32, flags: FdFlags) -> Result<i32> {
        if oldfd < 0 || newfd < 0 || newfd as usize >= self.max_files {
            crate::return_errno!(EBADF, "invalid file descriptor for dup2");
        }
        if oldfd == newfd {
            let _ = self.get(oldfd)?;
            return Ok(newfd);
        }
        let file = self.get(oldfd)?;
        let new_idx = newfd as usize;
        if new_idx >= self.files.len() {
            self.files.resize(new_idx + 1, None);
        } else if let Some(old_entry) = self.files[new_idx].take() {
            let _ = old_entry.file.close();
        }
        self.files[new_idx] = Some(FdEntry { file, flags });
        Ok(newfd)
    }

    /// Closes all file descriptors that have `FD_CLOEXEC` set.
    pub fn close_on_exec(&mut self) {
        for slot in self.files.iter_mut() {
            if slot.as_ref().is_some_and(|e| e.flags.contains(FdFlags::FD_CLOEXEC)) {
                let e = slot.take().unwrap();
                let _ = e.file.close();
            }
        }
    }

    /// Closes all open file descriptors.
    pub fn close_all(&mut self) {
        for slot in self.files.iter_mut() {
            if let Some(entry) = slot.take() {
                let _ = entry.file.close();
            }
        }
        self.files.clear();
    }

    /// Clones the descriptor table for `fork(2)`.
    pub fn clone_table(&self) -> Self {
        Self {
            files: self.files.clone(),
            cmask: self.cmask,
            max_files: self.max_files,
        }
    }
}

impl Default for Filedesc {
    fn default() -> Self {
        Self::new()
    }
}
