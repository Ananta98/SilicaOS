// SPDX-License-Identifier: GPL-2.0

//! Ext2 Superblock parsing and validation.

use crate::errno::{Errno, Result};

/// Standard Ext2 filesystem magic number (`0xEF53`).
pub const EXT2_SUPER_MAGIC: u16 = 0xEF53;

/// In-memory representation of an Ext2 Superblock.
#[derive(Clone, Copy, Debug)]
pub struct SuperBlock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub r_blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub log_frag_size: u32,
    pub blocks_per_group: u32,
    pub frags_per_group: u32,
    pub inodes_per_group: u32,
    pub mtime: u32,
    pub wtime: u32,
    pub mnt_count: u16,
    pub max_mnt_count: u16,
    pub magic: u16,
    pub state: u16,
    pub errors: u16,
    pub minor_rev_level: u16,
    pub lastcheck: u32,
    pub checkinterval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub def_resuid: u16,
    pub def_resgid: u16,
    pub first_ino: u32,
    pub inode_size: u16,
}

impl SuperBlock {
    /// Parses a 1024-byte superblock buffer read from offset 1024 on disk.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 1024 {
            return Err(Errno::EINVAL);
        }

        let magic = u16::from_le_bytes(buf[56..58].try_into().unwrap_or([0, 0]));
        if magic != EXT2_SUPER_MAGIC {
            return Err(Errno::EINVAL);
        }

        let inodes_count = u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4]));
        let blocks_count = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4]));
        let r_blocks_count = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));
        let free_blocks_count = u32::from_le_bytes(buf[12..16].try_into().unwrap_or([0; 4]));
        let free_inodes_count = u32::from_le_bytes(buf[16..20].try_into().unwrap_or([0; 4]));
        let first_data_block = u32::from_le_bytes(buf[20..24].try_into().unwrap_or([0; 4]));
        let log_block_size = u32::from_le_bytes(buf[24..28].try_into().unwrap_or([0; 4]));
        let log_frag_size = u32::from_le_bytes(buf[28..32].try_into().unwrap_or([0; 4]));
        let blocks_per_group = u32::from_le_bytes(buf[32..36].try_into().unwrap_or([0; 4]));
        let frags_per_group = u32::from_le_bytes(buf[36..40].try_into().unwrap_or([0; 4]));
        let inodes_per_group = u32::from_le_bytes(buf[40..44].try_into().unwrap_or([0; 4]));
        let mtime = u32::from_le_bytes(buf[44..48].try_into().unwrap_or([0; 4]));
        let wtime = u32::from_le_bytes(buf[48..52].try_into().unwrap_or([0; 4]));
        let mnt_count = u16::from_le_bytes(buf[52..54].try_into().unwrap_or([0; 2]));
        let max_mnt_count = u16::from_le_bytes(buf[54..56].try_into().unwrap_or([0; 2]));
        let state = u16::from_le_bytes(buf[58..60].try_into().unwrap_or([0; 2]));
        let errors = u16::from_le_bytes(buf[60..62].try_into().unwrap_or([0; 2]));
        let minor_rev_level = u16::from_le_bytes(buf[62..64].try_into().unwrap_or([0; 2]));
        let lastcheck = u32::from_le_bytes(buf[64..68].try_into().unwrap_or([0; 4]));
        let checkinterval = u32::from_le_bytes(buf[68..72].try_into().unwrap_or([0; 4]));
        let creator_os = u32::from_le_bytes(buf[72..76].try_into().unwrap_or([0; 4]));
        let rev_level = u32::from_le_bytes(buf[76..80].try_into().unwrap_or([0; 4]));
        let def_resuid = u16::from_le_bytes(buf[80..82].try_into().unwrap_or([0; 2]));
        let def_resgid = u16::from_le_bytes(buf[82..84].try_into().unwrap_or([0; 2]));

        let (first_ino, inode_size) = if rev_level >= 1 {
            let first_ino = u32::from_le_bytes(buf[84..88].try_into().unwrap_or([11, 0, 0, 0]));
            let inode_size = u16::from_le_bytes(buf[88..90].try_into().unwrap_or([128, 0]));
            (first_ino, inode_size)
        } else {
            (11, 128)
        };

        Ok(Self {
            inodes_count,
            blocks_count,
            r_blocks_count,
            free_blocks_count,
            free_inodes_count,
            first_data_block,
            log_block_size,
            log_frag_size,
            blocks_per_group,
            frags_per_group,
            inodes_per_group,
            mtime,
            wtime,
            mnt_count,
            max_mnt_count,
            magic,
            state,
            errors,
            minor_rev_level,
            lastcheck,
            checkinterval,
            creator_os,
            rev_level,
            def_resuid,
            def_resgid,
            first_ino,
            inode_size,
        })
    }

    /// Computes the filesystem block size in bytes (1024 << log_block_size).
    pub fn block_size(&self) -> usize {
        1024usize << self.log_block_size
    }

    /// Calculates the total number of block groups in this filesystem.
    pub fn block_group_count(&self) -> usize {
        if self.blocks_per_group == 0 {
            return 0;
        }
        ((self.blocks_count + self.blocks_per_group - 1) / self.blocks_per_group) as usize
    }
}
