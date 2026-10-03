// SPDX-License-Identifier: GPL-2.0

//! Ext2 Directory structure and parsing.

use alloc::{string::String, vec::Vec};
use crate::api::errno::{Errno, Result};

/// Parsed Ext2 directory entry.
#[derive(Clone, Debug)]
pub struct Ext2DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: String,
}

impl Ext2DirEntry {
    /// Attempts to parse a directory entry starting at `offset` in `buf`.
    pub fn parse(buf: &[u8], offset: usize) -> Result<Option<(Self, usize)>> {
        if offset + 8 > buf.len() {
            return Ok(None);
        }

        let inode = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap_or([0; 4]));
        let rec_len = u16::from_le_bytes(buf[offset + 4..offset + 6].try_into().unwrap_or([0; 2]));
        let name_len = buf[offset + 6];
        let file_type = buf[offset + 7];

        if rec_len < 8 || offset + (rec_len as usize) > buf.len() {
            return Err(Errno::EINVAL);
        }

        let next_offset = offset + (rec_len as usize);

        // Inode 0 signifies an unused/deleted directory entry slot
        if inode == 0 {
            return Ok(Some((
                Self {
                    inode: 0,
                    rec_len,
                    name_len: 0,
                    file_type: 0,
                    name: String::new(),
                },
                next_offset,
            )));
        }

        let name_end = offset + 8 + (name_len as usize);
        if name_end > offset + (rec_len as usize) {
            return Err(Errno::EINVAL);
        }

        let name_bytes = &buf[offset + 8..name_end];
        let name = core::str::from_utf8(name_bytes)
            .map(String::from)
            .unwrap_or_else(|_| String::from("?"));

        Ok(Some((
            Self {
                inode,
                rec_len,
                name_len,
                file_type,
                name,
            },
            next_offset,
        )))
    }
}

/// Finds an entry by name within the raw directory block data.
pub fn find_entry(dir_data: &[u8], target_name: &str) -> Option<u32> {
    let mut offset = 0;
    while let Ok(Some((entry, next_offset))) = Ext2DirEntry::parse(dir_data, offset) {
        if entry.inode != 0 && entry.name == target_name {
            return Some(entry.inode);
        }
        offset = next_offset;
    }
    None
}

/// Collects all valid directory entries within the raw directory block data.
pub fn list_entries(dir_data: &[u8]) -> Vec<Ext2DirEntry> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while let Ok(Some((entry, next_offset))) = Ext2DirEntry::parse(dir_data, offset) {
        if entry.inode != 0 {
            entries.push(entry);
        }
        offset = next_offset;
    }
    entries
}
