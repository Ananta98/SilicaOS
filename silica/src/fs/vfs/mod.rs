// SPDX-License-Identifier: GPL-2.0

//! Virtual File System (VFS) abstraction layer.

pub mod fs;
pub mod inode;
pub mod file;
pub mod perms;
pub mod dcache;
pub mod mount;
pub mod ops;
pub mod path;

pub use fs::*;
pub use inode::*;
pub use file::*;
pub use perms::*;
pub use dcache::*;
pub use mount::{Mount, PathNode, VFS_ROOT, root, set_root};
pub use ops::*;
pub use path::{LookupFlags, lookup, open};
