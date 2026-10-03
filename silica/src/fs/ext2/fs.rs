// SPDX-License-Identifier: GPL-2.0

//! Ext2 Filesystem driver implementation and VFS integration.

use alloc::{
    boxed::Box,
    sync::Arc,
    vec,
    vec::Vec,
};

use crate::{
    drivers::block::BlockDevice,
    api::errno::{Errno, Result},
    fs::{
        ext2::{
            block_group::BlockGroupDescriptor,
            dir::{find_entry, list_entries},
            inode::{Ext2RawInode, EXT2_ROOT_INO},
            superblock::SuperBlock,
        },
        vfs::{
            FileOps, FileSystem, INode, INodeAttr, NodeOps, OpenFlags, SeekAnchor,
        },
    },
};

/// High-level Ext2 filesystem instance backed by a block device.
pub struct Ext2Fs {
    pub dev: Arc<dyn BlockDevice>,
    pub sb: SuperBlock,
    pub block_size: usize,
    pub block_groups: Vec<BlockGroupDescriptor>,
}

impl Ext2Fs {
    /// Mounts an Ext2 filesystem from the given block device.
    pub fn new(dev: Arc<dyn BlockDevice>) -> Result<Arc<Self>> {
        let dev_bs = dev.block_size();
        if dev_bs == 0 || dev.capacity() < 2048 {
            return Err(Errno::EINVAL);
        }

        // 1. Read SuperBlock (at byte offset 1024)
        // Ensure buffer covers 1024..2048
        let sb_block_start = 1024 / dev_bs as u64;
        let blocks_to_read = ((1024 + dev_bs - 1) / dev_bs).max(1);
        let mut sb_raw = vec![0u8; blocks_to_read * dev_bs];
        dev.read_blocks(sb_block_start, &mut sb_raw)?;

        let sb_offset_in_buf = (1024 % dev_bs) as usize;
        let sb = SuperBlock::parse(&sb_raw[sb_offset_in_buf..sb_offset_in_buf + 1024])?;
        let block_size = sb.block_size();

        // 2. Read Block Group Descriptor Table
        let bg_count = sb.block_group_count();
        let mut block_groups = Vec::with_capacity(bg_count);

        let bg_desc_start_block = if block_size == 1024 { 2 } else { 1 };
        let descs_per_fs_block = block_size / 32;
        let total_bg_blocks = (bg_count + descs_per_fs_block - 1) / descs_per_fs_block;

        for b_idx in 0..total_bg_blocks {
            let mut bg_block = vec![0u8; block_size];
            Self::read_fs_block_direct(&*dev, block_size, bg_desc_start_block + b_idx as u32, &mut bg_block)?;

            let remaining_descs = bg_count - block_groups.len();
            let count = remaining_descs.min(descs_per_fs_block);
            for d in 0..count {
                let offset = d * 32;
                let desc = BlockGroupDescriptor::parse(&bg_block[offset..offset + 32])?;
                block_groups.push(desc);
            }
        }

        ostd::info!(
            "Ext2: Mounted filesystem (block_size: {} B, total_blocks: {}, inodes: {}, block_groups: {})",
            block_size,
            sb.blocks_count,
            sb.inodes_count,
            bg_count
        );

        Ok(Arc::new(Self {
            dev,
            sb,
            block_size,
            block_groups,
        }))
    }

    fn read_fs_block_direct(
        dev: &dyn BlockDevice,
        block_size: usize,
        block_num: u32,
        buf: &mut [u8],
    ) -> Result<()> {
        let dev_bs = dev.block_size();
        let sectors_per_fs_block = block_size / dev_bs;
        let start_sector = (block_num as u64) * (sectors_per_fs_block as u64);
        dev.read_blocks(start_sector, buf)?;
        Ok(())
    }

    /// Reads a single filesystem block by its logical block number.
    pub fn read_fs_block(&self, block_num: u32, buf: &mut [u8]) -> Result<()> {
        Self::read_fs_block_direct(&*self.dev, self.block_size, block_num, buf)
    }

    /// Reads an inode from disk by its inode number (1-indexed).
    pub fn read_inode(&self, ino: u32) -> Result<Ext2RawInode> {
        if ino == 0 || ino > self.sb.inodes_count {
            return Err(Errno::EINVAL);
        }

        let group = ((ino - 1) / self.sb.inodes_per_group) as usize;
        if group >= self.block_groups.len() {
            return Err(Errno::EINVAL);
        }

        let index_in_group = ((ino - 1) % self.sb.inodes_per_group) as usize;
        let inode_table_block = self.block_groups[group].inode_table;
        let inode_size = self.sb.inode_size as usize;

        let byte_offset = index_in_group * inode_size;
        let block_offset = (byte_offset / self.block_size) as u32;
        let offset_in_block = byte_offset % self.block_size;

        let mut block_buf = vec![0u8; self.block_size];
        self.read_fs_block(inode_table_block + block_offset, &mut block_buf)?;

        Ext2RawInode::parse(&block_buf[offset_in_block..offset_in_block + inode_size])
    }

    /// Reads data from a file inode across its mapped blocks.
    pub fn read_inode_data(&self, inode: &Ext2RawInode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file_size = inode.size();
        if offset >= file_size {
            return Ok(0);
        }

        let available = (file_size - offset) as usize;
        let to_read = buf.len().min(available);
        let mut read_bytes = 0;

        let mut block_cache = vec![0u8; self.block_size];

        while read_bytes < to_read {
            let curr_offset = offset + (read_bytes as u64);
            let logical_block = (curr_offset / self.block_size as u64) as u32;
            let offset_in_block = (curr_offset % self.block_size as u64) as usize;
            let chunk = (self.block_size - offset_in_block).min(to_read - read_bytes);

            let read_fn = |blk: u32, b: &mut [u8]| self.read_fs_block(blk, b);
            let phys_block = inode.get_block_num(logical_block, &read_fn, self.block_size)?;

            if phys_block == 0 {
                // Sparse block hole: fill with zeros
                buf[read_bytes..read_bytes + chunk].fill(0);
            } else {
                self.read_fs_block(phys_block, &mut block_cache)?;
                buf[read_bytes..read_bytes + chunk]
                    .copy_from_slice(&block_cache[offset_in_block..offset_in_block + chunk]);
            }

            read_bytes += chunk;
        }

        Ok(read_bytes)
    }

    /// Wraps an Ext2 inode in a VFS INode.
    pub fn get_vfs_inode(self: &Arc<Self>, ino: u32) -> Result<Arc<INode>> {
        let raw = self.read_inode(ino)?;
        let mode = raw.vfs_mode();
        let attr = INodeAttr {
            size: raw.size() as usize,
            mode,
            nlink: raw.links_count as usize,
            uid: raw.uid as u32,
            gid: raw.gid as u32,
            rdev: 0,
        };

        let node_ops: Box<dyn NodeOps> = if raw.is_dir() {
            Box::new(Ext2DirNodeOps {
                fs: Arc::clone(self),
                ino,
                raw,
            })
        } else {
            Box::new(Ext2FileNodeOps {
                fs: Arc::clone(self),
                ino,
                raw,
            })
        };

        Ok(Arc::new(INode::new_with_attr(node_ops, ino as usize, attr)))
    }
}

impl FileSystem for Ext2Fs {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn root(&self) -> Result<Arc<INode>> {
        let arc_self = Arc::new(Self {
            dev: Arc::clone(&self.dev),
            sb: self.sb,
            block_size: self.block_size,
            block_groups: self.block_groups.clone(),
        });
        arc_self.get_vfs_inode(EXT2_ROOT_INO)
    }
}

// ----------------------------------------------------------------------------
// Ext2 Directory and File NodeOps / FileOps
// ----------------------------------------------------------------------------

pub struct Ext2DirNodeOps {
    fs: Arc<Ext2Fs>,
    ino: u32,
    raw: Ext2RawInode,
}

impl NodeOps for Ext2DirNodeOps {
    fn lookup(&self, name: &str) -> Result<Arc<INode>> {
        let size = self.raw.size() as usize;
        let mut dir_data = vec![0u8; size];
        self.fs.read_inode_data(&self.raw, 0, &mut dir_data)?;

        let child_ino = find_entry(&dir_data, name).ok_or(Errno::ENOENT)?;
        self.fs.get_vfs_inode(child_ino)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        Ok(Box::new(Ext2DirFileOps {
            fs: Arc::clone(&self.fs),
            raw: self.raw,
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: self.raw.size() as usize,
            mode: self.raw.vfs_mode(),
            nlink: self.raw.links_count as usize,
            uid: self.raw.uid as u32,
            gid: self.raw.gid as u32,
            rdev: 0,
        })
    }
}

pub struct Ext2DirFileOps {
    fs: Arc<Ext2Fs>,
    raw: Ext2RawInode,
}

impl FileOps for Ext2DirFileOps {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot read raw bytes from directory");
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot write raw bytes to directory");
    }

    fn readdir(&self) -> Result<Vec<u8>> {
        let size = self.raw.size() as usize;
        let mut dir_data = vec![0u8; size];
        self.fs.read_inode_data(&self.raw, 0, &mut dir_data)?;

        let entries = list_entries(&dir_data);
        let mut out = Vec::new();
        for e in entries {
            out.extend_from_slice(e.name.as_bytes());
            out.push(b'\n');
        }
        Ok(out)
    }
}

pub struct Ext2FileNodeOps {
    fs: Arc<Ext2Fs>,
    ino: u32,
    raw: Ext2RawInode,
}

impl NodeOps for Ext2FileNodeOps {
    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        if flags.contains(OpenFlags::WRITE) {
            crate::return_errno!(EROFS, "ext2 write support not implemented");
        }
        Ok(Box::new(Ext2FileOps {
            fs: Arc::clone(&self.fs),
            raw: self.raw,
        }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: self.raw.size() as usize,
            mode: self.raw.vfs_mode(),
            nlink: self.raw.links_count as usize,
            uid: self.raw.uid as u32,
            gid: self.raw.gid as u32,
            rdev: 0,
        })
    }
}

pub struct Ext2FileOps {
    fs: Arc<Ext2Fs>,
    raw: Ext2RawInode,
}

impl FileOps for Ext2FileOps {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.fs.read_inode_data(&self.raw, offset, buf)
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EROFS, "ext2 filesystem is currently mounted read-only");
    }

    fn seek(&self, curr_offset: u64, anchor: SeekAnchor) -> Result<u64> {
        let file_size = self.raw.size() as i64;
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
