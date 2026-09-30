// SPDX-License-Identifier: GPL-2.0

//! ExFAT Filesystem driver and VFS integration.

use alloc::{
    boxed::Box,
    sync::Arc,
    vec,
    vec::Vec,
};

use crate::{
    drivers::block::BlockDevice,
    errno::{Errno, Result},
    fs::{
        exfat::{
            boot_sector::ExFatBootSector,
            cluster::ClusterManager,
            entry::{parse_directory, ExFatFileEntry},
        },
        vfs::{
            FileOps, FileSystem, INode, INodeAttr, Mode, NodeOps, OpenFlags, SeekAnchor,
        },
    },
};

/// High-level ExFAT filesystem instance backed by a block device.
pub struct ExFatFs {
    pub dev: Arc<dyn BlockDevice>,
    pub bs: ExFatBootSector,
    pub cluster_mgr: ClusterManager,
}

impl ExFatFs {
    /// Mounts an ExFAT filesystem from the given block device.
    pub fn new(dev: Arc<dyn BlockDevice>) -> Result<Arc<Self>> {
        let dev_bs = dev.block_size();
        if dev_bs == 0 || dev.capacity() < 512 {
            return Err(Errno::EINVAL);
        }

        // 1. Read Sector 0 (Volume Boot Record)
        let mut vbr_buf = vec![0u8; dev_bs.max(512)];
        dev.read_blocks(0, &mut vbr_buf)?;

        let bs = ExFatBootSector::parse(&vbr_buf[..512])?;
        let cluster_mgr = ClusterManager::new(Arc::clone(&dev), bs);

        ostd::info!(
            "ExFAT: Mounted volume (sector_size: {} B, cluster_size: {} B, root_cluster: {})",
            bs.sector_size(),
            bs.cluster_size(),
            bs.root_dir_first_cluster
        );

        Ok(Arc::new(Self {
            dev,
            bs,
            cluster_mgr,
        }))
    }

    /// Reads directory entries for a directory starting at `first_cluster`.
    pub fn read_directory_entries(
        &self,
        first_cluster: u32,
        no_fat_chain: bool,
    ) -> Result<Vec<ExFatFileEntry>> {
        // Collect clusters for this directory
        let cluster_size = self.bs.cluster_size();
        let mut dir_bytes = Vec::new();
        let mut curr_cluster = first_cluster;

        let mut cluster_buf = vec![0u8; cluster_size];
        let mut visited = 0;

        while visited < 1024 {
            self.cluster_mgr.read_cluster(curr_cluster, &mut cluster_buf)?;
            dir_bytes.extend_from_slice(&cluster_buf);

            // Check if end of directory marker exists in this cluster
            let mut found_eod = false;
            for i in 0..(cluster_size / 32) {
                if cluster_buf[i * 32] == 0x00 {
                    found_eod = true;
                    break;
                }
            }
            if found_eod {
                break;
            }

            if let Some(next) = self.cluster_mgr.next_cluster(curr_cluster, no_fat_chain)? {
                curr_cluster = next;
                visited += 1;
            } else {
                break;
            }
        }

        parse_directory(&dir_bytes)
    }

    /// Creates a VFS INode for an ExFAT entry.
    pub fn create_vfs_inode(self: &Arc<Self>, entry: ExFatFileEntry, id: usize) -> Arc<INode> {
        let is_dir = entry.is_dir;
        let size = entry.data_length as usize;
        let mode = if is_dir {
            Mode::DIR | Mode::READ | Mode::EXEC
        } else {
            Mode::FILE | Mode::READ
        };

        let attr = INodeAttr {
            size,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        };

        let node_ops: Box<dyn NodeOps> = if is_dir {
            Box::new(ExFatDirNodeOps {
                fs: Arc::clone(self),
                entry,
            })
        } else {
            Box::new(ExFatFileNodeOps {
                fs: Arc::clone(self),
                entry,
            })
        };

        Arc::new(INode::new_with_attr(node_ops, id, attr))
    }
}

impl FileSystem for ExFatFs {
    fn name(&self) -> &'static str {
        "exfat"
    }

    fn root(&self) -> Result<Arc<INode>> {
        let root_entry = ExFatFileEntry {
            name: "/".into(),
            is_dir: true,
            first_cluster: self.bs.root_dir_first_cluster,
            data_length: 0,
            no_fat_chain: false,
        };
        let arc_self = Arc::new(Self {
            dev: Arc::clone(&self.dev),
            bs: self.bs,
            cluster_mgr: ClusterManager::new(Arc::clone(&self.dev), self.bs),
        });
        Ok(arc_self.create_vfs_inode(root_entry, 1))
    }
}

// ----------------------------------------------------------------------------
// ExFAT VFS NodeOps & FileOps
// ----------------------------------------------------------------------------

pub struct ExFatDirNodeOps {
    fs: Arc<ExFatFs>,
    entry: ExFatFileEntry,
}

impl NodeOps for ExFatDirNodeOps {
    fn lookup(&self, name: &str) -> Result<Arc<INode>> {
        let entries = self
            .fs
            .read_directory_entries(self.entry.first_cluster, self.entry.no_fat_chain)?;

        for (idx, e) in entries.into_iter().enumerate() {
            if e.name == name {
                return Ok(self.fs.create_vfs_inode(e, idx + 2));
            }
        }

        Err(Errno::ENOENT)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(ExFatDirFileOps {
            fs: Arc::clone(&self.fs),
            entry: self.entry.clone(),
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: 0,
            mode: Mode::DIR | Mode::READ | Mode::EXEC,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

pub struct ExFatDirFileOps {
    fs: Arc<ExFatFs>,
    entry: ExFatFileEntry,
}

impl FileOps for ExFatDirFileOps {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot read raw bytes from directory");
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot write raw bytes to directory");
    }

    fn readdir(&self) -> Result<Vec<u8>> {
        let entries = self
            .fs
            .read_directory_entries(self.entry.first_cluster, self.entry.no_fat_chain)?;

        let mut out = Vec::new();
        for e in entries {
            out.extend_from_slice(e.name.as_bytes());
            out.push(b'\n');
        }
        Ok(out)
    }
}

pub struct ExFatFileNodeOps {
    fs: Arc<ExFatFs>,
    entry: ExFatFileEntry,
}

impl NodeOps for ExFatFileNodeOps {
    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        if flags.contains(OpenFlags::WRITE) {
            crate::return_errno!(EROFS, "exfat write support not implemented");
        }
        Ok(Box::new(ExFatFileOps {
            fs: Arc::clone(&self.fs),
            entry: self.entry.clone(),
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: self.entry.data_length as usize,
            mode: Mode::FILE | Mode::READ,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

pub struct ExFatFileOps {
    fs: Arc<ExFatFs>,
    entry: ExFatFileEntry,
}

impl FileOps for ExFatFileOps {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.fs.cluster_mgr.read_chain(
            self.entry.first_cluster,
            self.entry.no_fat_chain,
            offset,
            self.entry.data_length,
            buf,
        )
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EROFS, "exfat filesystem is currently mounted read-only");
    }

    fn seek(&self, curr_offset: u64, anchor: SeekAnchor) -> Result<u64> {
        let file_size = self.entry.data_length as i64;
        let new_offset = match anchor {
            SeekAnchor::Start(pos) => pos as i64,
            SeekAnchor::Current(delta) => curr_offset as i64 + delta,
            SeekAnchor::End(delta) => file_size + delta,
        };
        if new_offset < 0 {
            crate::return_errno!(EINVAL, "invalid seek position");
        }
        Ok(new_offset as u64)
    }
}
