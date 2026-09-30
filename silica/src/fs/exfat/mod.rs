// SPDX-License-Identifier: GPL-2.0

//! ExFAT Filesystem Driver Module.
//!
//! Provides read-only Extended FAT (ExFAT) filesystem support over [`BlockDevice`].

pub mod boot_sector;
pub mod cluster;
pub mod entry;
pub mod fs;

use alloc::sync::Arc;

pub use boot_sector::{ExFatBootSector, EXFAT_FS_NAME};
pub use cluster::ClusterManager;
pub use entry::ExFatFileEntry;
pub use fs::ExFatFs;

use crate::{
    drivers::block::BlockDevice,
    errno::Result,
    fs::{
        registry::{register_filesystem_type, FileSystemType},
        vfs::FileSystem,
    },
    modules::KernelModule,
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

/// ExFAT Filesystem Kernel Module.
pub struct ExFatModule;

impl KernelModule for ExFatModule {
    fn name(&self) -> &'static str {
        "exfat"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn description(&self) -> &'static str {
        "Extended File Allocation Table (ExFAT) Filesystem Driver"
    }

    fn author(&self) -> &'static str {
        "SilicaOS Team"
    }

    fn init(&self) -> Result<()> {
        register_filesystem_type(Arc::new(ExFatFsType));
        Ok(())
    }

    fn exit(&self) -> Result<()> {
        ostd::info!("ExFAT: Driver unloaded");
        Ok(())
    }
}
