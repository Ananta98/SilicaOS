// SPDX-License-Identifier: GPL-2.0

//! Ext2 Inode representation and block mapping.

use alloc::vec;
use crate::{
    errno::{Errno, Result},
    fs::vfs::Mode,
};

/// The reserved inode number for the root directory of an Ext2 filesystem.
pub const EXT2_ROOT_INO: u32 = 2;

/// Ext2 on-disk Inode.
#[derive(Clone, Copy, Debug)]
pub struct Ext2RawInode {
    pub mode: u16,
    pub uid: u16,
    pub size_low: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub osd1: u32,
    pub block: [u32; 15],
    pub generation: u32,
    pub file_acl: u32,
    pub dir_acl_or_size_high: u32,
    pub faddr: u32,
}

impl Ext2RawInode {
    /// Parses an Ext2 raw inode from raw byte slice (at least 128 bytes).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 128 {
            return Err(Errno::EINVAL);
        }

        let mode = u16::from_le_bytes(buf[0..2].try_into().unwrap_or([0; 2]));
        let uid = u16::from_le_bytes(buf[2..4].try_into().unwrap_or([0; 2]));
        let size_low = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4]));
        let atime = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));
        let ctime = u32::from_le_bytes(buf[12..16].try_into().unwrap_or([0; 4]));
        let mtime = u32::from_le_bytes(buf[16..20].try_into().unwrap_or([0; 4]));
        let dtime = u32::from_le_bytes(buf[20..24].try_into().unwrap_or([0; 4]));
        let gid = u16::from_le_bytes(buf[24..26].try_into().unwrap_or([0; 2]));
        let links_count = u16::from_le_bytes(buf[26..28].try_into().unwrap_or([0; 2]));
        let blocks = u32::from_le_bytes(buf[28..32].try_into().unwrap_or([0; 4]));
        let flags = u32::from_le_bytes(buf[32..36].try_into().unwrap_or([0; 4]));
        let osd1 = u32::from_le_bytes(buf[36..40].try_into().unwrap_or([0; 4]));

        let mut block = [0u32; 15];
        for i in 0..15 {
            let offset = 40 + i * 4;
            block[i] = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap_or([0; 4]));
        }

        let generation = u32::from_le_bytes(buf[100..104].try_into().unwrap_or([0; 4]));
        let file_acl = u32::from_le_bytes(buf[104..108].try_into().unwrap_or([0; 4]));
        let dir_acl_or_size_high = u32::from_le_bytes(buf[108..112].try_into().unwrap_or([0; 4]));
        let faddr = u32::from_le_bytes(buf[112..116].try_into().unwrap_or([0; 4]));

        Ok(Self {
            mode,
            uid,
            size_low,
            atime,
            ctime,
            mtime,
            dtime,
            gid,
            links_count,
            blocks,
            flags,
            osd1,
            block,
            generation,
            file_acl,
            dir_acl_or_size_high,
            faddr,
        })
    }

    /// Returns the full 64-bit file size.
    pub fn size(&self) -> u64 {
        if self.is_dir() {
            self.size_low as u64
        } else {
            (self.size_low as u64) | ((self.dir_acl_or_size_high as u64) << 32)
        }
    }

    /// Checks if this inode is a directory.
    pub fn is_dir(&self) -> bool {
        (self.mode & 0xF000) == 0x4000
    }

    /// Checks if this inode is a regular file.
    pub fn is_file(&self) -> bool {
        (self.mode & 0xF000) == 0x8000
    }

    /// Converts Ext2 mode to VFS Mode.
    pub fn vfs_mode(&self) -> Mode {
        let mut m = Mode::from_bits_truncate((self.mode & 0x0FFF) as u32);
        if self.is_dir() {
            m |= Mode::DIR;
        } else if self.is_file() {
            m |= Mode::FILE;
        }
        m
    }

    /// Resolves the physical block number on disk for a given logical block within the file.
    pub fn get_block_num(
        &self,
        logical_block: u32,
        read_fs_block: &dyn Fn(u32, &mut [u8]) -> Result<()>,
        ext2_block_size: usize,
    ) -> Result<u32> {
        let ptrs_per_block = (ext2_block_size / 4) as u32;

        // 1. Direct blocks (0..12)
        if logical_block < 12 {
            return Ok(self.block[logical_block as usize]);
        }

        let mut rel = logical_block - 12;

        // 2. Single indirect block (12)
        if rel < ptrs_per_block {
            let indirect_block = self.block[12];
            if indirect_block == 0 {
                return Ok(0);
            }
            let mut block_data = vec![0u8; ext2_block_size];
            read_fs_block(indirect_block, &mut block_data)?;
            let offset = (rel as usize) * 4;
            let blk = u32::from_le_bytes(block_data[offset..offset + 4].try_into().unwrap_or([0; 4]));
            return Ok(blk);
        }
        rel -= ptrs_per_block;

        // 3. Double indirect block (13)
        let double_capacity = ptrs_per_block * ptrs_per_block;
        if rel < double_capacity {
            let double_block = self.block[13];
            if double_block == 0 {
                return Ok(0);
            }
            let mut double_data = vec![0u8; ext2_block_size];
            read_fs_block(double_block, &mut double_data)?;

            let first_idx = (rel / ptrs_per_block) as usize;
            let second_idx = (rel % ptrs_per_block) as usize;

            let indirect_block = u32::from_le_bytes(
                double_data[first_idx * 4..first_idx * 4 + 4].try_into().unwrap_or([0; 4]),
            );
            if indirect_block == 0 {
                return Ok(0);
            }

            let mut single_data = vec![0u8; ext2_block_size];
            read_fs_block(indirect_block, &mut single_data)?;

            let blk = u32::from_le_bytes(
                single_data[second_idx * 4..second_idx * 4 + 4].try_into().unwrap_or([0; 4]),
            );
            return Ok(blk);
        }

        // Exceeds double indirect limits
        Err(Errno::EOVERFLOW)
    }
}
