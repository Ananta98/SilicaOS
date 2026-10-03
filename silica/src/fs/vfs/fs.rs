// SPDX-License-Identifier: GPL-2.0

//! Core FileSystem trait for the VFS.

use alloc::sync::Arc;
use crate::api::errno::Result;
use super::inode::INode;

/// The common trait implemented by every mounted filesystem driver in SilicaOS.
pub trait FileSystem: Send + Sync {
    /// Returns the human-readable name or driver type (e.g. "initramfs", "devfs", "ext2").
    fn name(&self) -> &'static str;

    /// Retrieves the root INode of this filesystem.
    fn root(&self) -> Result<Arc<INode>>;
}
