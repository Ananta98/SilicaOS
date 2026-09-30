// SPDX-License-Identifier: GPL-2.0

//! Ext2 Block Group Descriptor structure and table management.

use crate::errno::{Errno, Result};

/// Size of an on-disk Ext2 Block Group Descriptor in bytes.
pub const EXT2_BG_DESC_SIZE: usize = 32;

/// Ext2 Block Group Descriptor.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockGroupDescriptor {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}

impl BlockGroupDescriptor {
    /// Parses a 32-byte block group descriptor slice.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < EXT2_BG_DESC_SIZE {
            return Err(Errno::EINVAL);
        }

        let block_bitmap = u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4]));
        let inode_bitmap = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4]));
        let inode_table = u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4]));
        let free_blocks_count = u16::from_le_bytes(buf[12..14].try_into().unwrap_or([0; 2]));
        let free_inodes_count = u16::from_le_bytes(buf[14..16].try_into().unwrap_or([0; 2]));
        let used_dirs_count = u16::from_le_bytes(buf[16..18].try_into().unwrap_or([0; 2]));

        Ok(Self {
            block_bitmap,
            inode_bitmap,
            inode_table,
            free_blocks_count,
            free_inodes_count,
            used_dirs_count,
        })
    }
}
