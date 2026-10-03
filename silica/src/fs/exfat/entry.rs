// SPDX-License-Identifier: GPL-2.0

//! ExFAT Directory Entry structures and set parsing.

use alloc::{string::String, vec::Vec};
use crate::api::errno::Result;

pub const EXFAT_ENTRY_EOD: u8 = 0x00;
pub const EXFAT_ENTRY_FILE: u8 = 0x85;
pub const EXFAT_ENTRY_STREAM: u8 = 0xC0;
pub const EXFAT_ENTRY_NAME: u8 = 0xC1;

pub const EXFAT_ATTR_DIRECTORY: u16 = 0x0010;

/// Parsed representation of an ExFAT file or directory record.
#[derive(Clone, Debug)]
pub struct ExFatFileEntry {
    pub name: String,
    pub is_dir: bool,
    pub first_cluster: u32,
    pub data_length: u64,
    pub no_fat_chain: bool,
}

/// Parses all active file/directory records from the directory data buffer.
pub fn parse_directory(dir_data: &[u8]) -> Result<Vec<ExFatFileEntry>> {
    let mut entries = Vec::new();
    let num_records = dir_data.len() / 32;
    let mut i = 0;

    while i < num_records {
        let offset = i * 32;
        let entry_type = dir_data[offset];

        if entry_type == EXFAT_ENTRY_EOD {
            break;
        }

        // Check if this is a File Directory Entry
        if entry_type == EXFAT_ENTRY_FILE {
            let secondary_count = dir_data[offset + 1] as usize;
            let file_attrs = u16::from_le_bytes(
                dir_data[offset + 4..offset + 6].try_into().unwrap_or([0; 2]),
            );
            let is_dir = (file_attrs & EXFAT_ATTR_DIRECTORY) != 0;

            if i + secondary_count >= num_records {
                break;
            }

            // Next entry must be Stream Extension (0xC0)
            let stream_offset = (i + 1) * 32;
            if dir_data[stream_offset] == EXFAT_ENTRY_STREAM {
                let stream_flags = dir_data[stream_offset + 1];
                let no_fat_chain = (stream_flags & 0x02) != 0;
                let first_cluster = u32::from_le_bytes(
                    dir_data[stream_offset + 20..stream_offset + 24]
                        .try_into()
                        .unwrap_or([0; 4]),
                );
                let data_length = u64::from_le_bytes(
                    dir_data[stream_offset + 24..stream_offset + 32]
                        .try_into()
                        .unwrap_or([0; 8]),
                );

                // Subsequent entries are File Name entries (0xC1)
                let mut utf16_chars = Vec::new();
                for sec in 2..=secondary_count {
                    let name_offset = (i + sec) * 32;
                    if dir_data[name_offset] == EXFAT_ENTRY_NAME {
                        for c in 0..15 {
                            let char_offset = name_offset + 2 + c * 2;
                            let code_point = u16::from_le_bytes(
                                dir_data[char_offset..char_offset + 2]
                                    .try_into()
                                    .unwrap_or([0; 2]),
                            );
                            if code_point == 0 {
                                break;
                            }
                            utf16_chars.push(code_point);
                        }
                    }
                }

                let name = String::from_utf16_lossy(&utf16_chars);
                if !name.is_empty() {
                    entries.push(ExFatFileEntry {
                        name,
                        is_dir,
                        first_cluster,
                        data_length,
                        no_fat_chain,
                    });
                }
            }

            i += 1 + secondary_count;
        } else {
            i += 1;
        }
    }

    Ok(entries)
}
