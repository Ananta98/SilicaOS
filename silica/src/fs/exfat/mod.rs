// SPDX-License-Identifier: GPL-2.0

//! ExFAT Filesystem Driver Module.
//!
//! Provides read-only Extended FAT (ExFAT) filesystem support over [`BlockDevice`].

pub mod boot_sector;
pub mod cluster;
pub mod entry;
pub mod fs;

use alloc::sync::Arc;

pub use boot_sector::{EXFAT_FS_NAME, ExFatBootSector};
pub use cluster::ClusterManager;
pub use entry::ExFatFileEntry;
pub use fs::ExFatFs;

use crate::{
    drivers::block::BlockDevice,
    api::errno::Result,
    fs::{
        registry::{FileSystemType, register_filesystem_type},
        vfs::FileSystem,
    },
};

/// Filesystem type descriptor for ExFAT.
pub struct ExFatFsType;

impl FileSystemType for ExFatFsType {
    fn name(&self) -> &'static str {
        "exfat"
    }

    fn mount(&self, dev: Arc<dyn BlockDevice>) -> Result<Arc<dyn FileSystem>> {
        let fs = ExFatFs::new(dev)?;
        Ok(fs as _)
    }
}

fn init() -> Result<()> {
    register_filesystem_type(Arc::new(ExFatFsType));
    Ok(())
}

crate::module!(
    "ExFAT Filesystem Driver",
    "Ananta98",
    crate::modules::InitcallLevel::Fs,
    init
);
