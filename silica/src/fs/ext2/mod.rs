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
pub use inode::{Ext2RawInode, EXT2_ROOT_INO};
pub use superblock::{SuperBlock, EXT2_SUPER_MAGIC};

use crate::{
    drivers::block::BlockDevice,
    errno::Result,
    fs::{
        registry::{register_filesystem_type, FileSystemType},
        vfs::FileSystem,
    },
    modules::KernelModule,
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

/// Ext2 Filesystem Kernel Module.
pub struct Ext2Module;

impl KernelModule for Ext2Module {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn description(&self) -> &'static str {
        "Second Extended (Ext2) Filesystem Driver"
    }

    fn author(&self) -> &'static str {
        "SilicaOS Team"
    }

    fn init(&self) -> Result<()> {
        register_filesystem_type(Arc::new(Ext2FsType));
        Ok(())
    }

    fn exit(&self) -> Result<()> {
        ostd::info!("Ext2: Driver unloaded");
        Ok(())
    }
}
