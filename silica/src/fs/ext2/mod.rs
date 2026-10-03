// SPDX-License-Identifier: GPL-2.0

//! Ext2 Filesystem Driver Module.
//!
//! Provides read-only Ext2 filesystem support over [`BlockDevice`].

pub mod block_group;
pub mod dir;
pub mod fs;
pub mod inode;
pub mod superblock;

use alloc::sync::Arc;

pub use block_group::BlockGroupDescriptor;
pub use dir::Ext2DirEntry;
pub use fs::Ext2Fs;
pub use inode::{EXT2_ROOT_INO, Ext2RawInode};
pub use superblock::{EXT2_SUPER_MAGIC, SuperBlock};

use crate::{
    drivers::block::BlockDevice,
    api::errno::Result,
    fs::{
        registry::{FileSystemType, register_filesystem_type},
        vfs::FileSystem,
    },
};

/// Filesystem type descriptor for Ext2.
pub struct Ext2FsType;

impl FileSystemType for Ext2FsType {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn mount(&self, dev: Arc<dyn BlockDevice>) -> Result<Arc<dyn FileSystem>> {
        let fs = Ext2Fs::new(dev)?;
        Ok(fs as _)
    }
}

fn init() -> Result<()> {
    register_filesystem_type(Arc::new(Ext2FsType));
    Ok(())
}

crate::module!(
    "Ext2 Filesystem Driver",
    "Ananta98",
    crate::modules::InitcallLevel::Fs,
    init
);
